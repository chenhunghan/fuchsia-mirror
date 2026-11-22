// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::mem::{align_of, offset_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use kprint::kprintln;
use zx_status::Status;
#[cfg(console_enabled)]
use zx_types::ZX_OK;

use super::apic::ApicInterruptDeliveryMode;
use super::descriptor::{self, Tss64};
use super::feature::{
    self, X86_MAX_CSTATES, X86CpuidLeafNum, X86VendorList, x86_idle_state_t, x86_idle_states_t,
};
use super::idle_states::{
    X86_IDLE_STATE_MASK_C1_ONLY, X86IdleStates, x86_cstate_c1, x86_get_idle_states,
};
use super::interrupts::{
    X86_INT_DOUBLE_FAULT, X86_INT_IPI_GENERIC, X86_INT_IPI_HALT, X86_INT_IPI_INTERRUPT,
    X86_INT_IPI_RESCHEDULE, X86_INT_MACHINE_CHECK, X86_INT_NMI,
};
use super::lapic::{
    INVALID_APIC_ID, apic_bsp_id, apic_local_id, apic_send_broadcast_ipi,
    apic_send_broadcast_self_ipi, apic_send_ipi, apic_send_mask_ipi,
};
use super::mp_ffi::x86_enable_ints_and_hlt;

use super::mwait_monitor::{MwaitMonitor, MwaitMonitorArray};
use super::registers::{
    self, ExtendedRegisterFeature, X86_CR4_FSGSBASE, X86_CR4_UMIP, X86_EFER_SCE, X86_FLAGS_AC,
    X86_FLAGS_IOPL_MASK, X86_FLAGS_NT, X86_FLAGS_STATUS_MASK, X86_MSR_IA32_EFER,
    X86_MSR_IA32_FMASK, X86_MSR_IA32_GS_BASE, X86_MSR_IA32_KERNEL_GS_BASE, X86_MSR_IA32_LSTAR,
    X86_MSR_IA32_STAR, X86_MSR_IA32_SYSENTER_CS, X86_MSR_IA32_TSC_AUX,
};
use super::x86::{
    read_msr, wbinvd, write_msr, x86_cli, x86_enable_ints_and_mwait, x86_get_cr4, x86_hlt,
    x86_ras_fill, x86_read_gs_offset32, x86_read_gs_offset64, x86_restore_flags, x86_save_flags,
    x86_set_cr4,
};
use super::{CODE_64_SELECTOR, MAX_CACHE_LINE, USER_CODE_SELECTOR, idt};

#[repr(C)]
pub struct CpuidIo(pub core::cell::UnsafeCell<[u32; 4]>);
// SAFETY: `CpuidIo` is only used for read-only boot CPUID data after initialization.
unsafe impl Sync for CpuidIo {}

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::mp::{MpIpi, MpIpiTarget, MpUnplugEvent, cpu_num_to_mask, is_cpu_online};
use crate::kernel::thread::Thread;
use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t, cpu_num_t, lowest_cpu_set};

const LOCAL_KTRACE_ENABLE: bool = false;
const NMI_IST_INDEX: u8 = 1;
const MCE_IST_INDEX: u8 = 2;
const DBF_IST_INDEX: u8 = 3;

/// Offsets into [`X86Percpu`], used by assembly and `%gs`-relative accessors.
pub const PERCPU_DIRECT_OFFSET: usize = 0x0;
pub const PERCPU_CURRENT_THREAD_OFFSET: usize = 0x8;
pub const ZX_TLS_STACK_GUARD_OFFSET: usize = 0x10;
pub const ZX_TLS_UNSAFE_SP_OFFSET: usize = 0x18;
pub const PERCPU_SAVED_USER_SP_OFFSET: usize = 0x20;
pub const PERCPU_IN_RESTRICTED_MODE: usize = 0x28;
pub const PERCPU_GPF_RETURN_OFFSET: usize = 0x50;
pub const PERCPU_CPU_NUM_OFFSET: usize = 0x58;
pub const PERCPU_NUM_SPINLOCKS_OFFSET: usize = 0x5c;
pub const PERCPU_LAST_USER_ASPACE_OFFSET: usize = 0x60;
pub const PERCPU_HIGH_LEVEL_PERCPU_OFFSET: usize = 0x68;
pub const PERCPU_DEFAULT_TSS_OFFSET: usize = 0x70;
pub const PERCPU_KERNEL_SP_OFFSET: usize = PERCPU_DEFAULT_TSS_OFFSET + 4;
pub const PERCPU_INTERRUPT_STACKS_NMI_OFFSET: usize = 0x20e0;

pub const INTERRUPT_STACK_SIZE: usize = 4096;

/// Reserved space for special interrupt stacks (`x86_percpu::interrupt_stacks`).
#[repr(C, align(16))]
pub struct InterruptStacks {
    pub nmi: [u8; INTERRUPT_STACK_SIZE],
    pub machine_check: [u8; INTERRUPT_STACK_SIZE],
    pub double_fault: [u8; INTERRUPT_STACK_SIZE],
}

#[derive(Debug, Default)]
#[repr(C, align(16))]
pub struct Align16;

/// Per-CPU structure pointed to by `%gs:` in the kernel (`x86_percpu`).
///
/// This struct is accessed very early during boot so it lives in BSS/data and is initialized
/// without calling global constructors.
#[repr(C, align(64))]
pub struct X86Percpu {
    /// A direct pointer to ourselves.
    pub direct: *mut X86Percpu,

    /// The current thread.
    pub current_thread: *mut Thread,

    /// The offsets of these two slots are published in `system/public/zircon/tls.h` and known to
    /// the compiler.
    pub stack_guard: usize,
    pub kernel_unsafe_sp: usize,

    /// Temporarily saved during a syscall.
    pub saved_user_sp: usize,

    /// Flag to track that we're in restricted mode.
    pub in_restricted_mode: u32,

    /// Whether blocking is disallowed. See `arch_blocking_disallowed()`.
    pub blocking_disallowed: u32,

    /// Used for IPI-free rescheduling of idle CPUs with monitor/mwait.
    pub monitor: *const MwaitMonitor,

    /// Interlock to avoid HLT on idle CPUs without monitor/mwait.
    /// `halt_interlock` is never used on CPUs that have enabled monitor/mwait for idle.
    pub halt_interlock: AtomicU32,

    /// Supported mwait C-states for idle CPUs.
    pub idle_states: *const X86IdleStates,

    /// Local APIC id.
    pub apic_id: u32,

    /// If nonzero and we receive a GPF, change the return IP to this value.
    pub gpf_return_target: usize,

    /// CPU number.
    pub cpu_num: cpu_num_t,

    /// Number of spinlocks currently held.
    pub num_spinlocks: u32,

    /// Last user `VmAspace` that was active on this core. Lazily updated.
    pub last_user_aspace: *mut c_void,

    /// A pointer providing fast access to the high-level arch-agnostic per-cpu struct.
    pub high_level_percpu: *mut c_void,

    /// Zero-sized 16-byte alignment marker used to enforce `__ALIGNED(16)` on `default_tss`.
    pub _default_tss_align: Align16,

    /// This CPU's default TSS. Requires align(16) to match C++ x86_percpu.
    pub default_tss: Tss64,

    /// Reserved space for special interrupt stacks.
    pub interrupt_stacks: InterruptStacks,
}

zr::static_assert!(size_of::<X86Percpu>() == 0x5100);
zr::static_assert!(align_of::<X86Percpu>() == 64);
zr::static_assert!(size_of::<InterruptStacks>() == 3 * INTERRUPT_STACK_SIZE);

zr::static_assert!(offset_of!(X86Percpu, direct) == PERCPU_DIRECT_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, current_thread) == PERCPU_CURRENT_THREAD_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, stack_guard) == ZX_TLS_STACK_GUARD_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, kernel_unsafe_sp) == ZX_TLS_UNSAFE_SP_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, saved_user_sp) == PERCPU_SAVED_USER_SP_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, in_restricted_mode) == PERCPU_IN_RESTRICTED_MODE);
zr::static_assert!(offset_of!(X86Percpu, gpf_return_target) == PERCPU_GPF_RETURN_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, cpu_num) == PERCPU_CPU_NUM_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, num_spinlocks) == PERCPU_NUM_SPINLOCKS_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, last_user_aspace) == PERCPU_LAST_USER_ASPACE_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, high_level_percpu) == PERCPU_HIGH_LEVEL_PERCPU_OFFSET);
zr::static_assert!(offset_of!(X86Percpu, default_tss) == PERCPU_DEFAULT_TSS_OFFSET);
zr::static_assert!(
    PERCPU_DEFAULT_TSS_OFFSET + offset_of!(super::descriptor::Tss64, rsp0)
        == PERCPU_KERNEL_SP_OFFSET
);
zr::static_assert!(
    offset_of!(X86Percpu, interrupt_stacks) + offset_of!(InterruptStacks, nmi)
        == PERCPU_INTERRUPT_STACKS_NMI_OFFSET
);

struct SyncApPercpus(UnsafeCell<(*mut X86Percpu, usize)>);

// SAFETY: `AP_PERCPUS` is populated once on the boot CPU in `allocate_ap_structures` before
// secondary CPUs are started, and the slice pointer/length are read-only thereafter.
unsafe impl Sync for SyncApPercpus {}

static AP_PERCPUS: SyncApPercpus = SyncApPercpus(UnsafeCell::new((ptr::null_mut(), 0)));

/// Number of CPUs on the system (`x86_num_cpus`). Written once on the boot CPU in
/// [`allocate_ap_structures`] before secondary CPUs are started, and read-only thereafter.
#[repr(transparent)]
pub struct BootGlobal<T>(core::cell::UnsafeCell<T>);

// SAFETY: `BootGlobal` is written once on the boot CPU during early initialization and read-only
// thereafter across multiple CPUs. The inner type `T` must also be thread-safe.
unsafe impl<T: Send + Sync> Sync for BootGlobal<T> {}

// SAFETY: Safe to export this symbol to C++ because `X86_NUM_CPUS` is a valid symbol and
// `BootGlobal<u8>` has standard layout compatible with `uint8_t`.
#[unsafe(export_name = "x86_num_cpus")]
pub static X86_NUM_CPUS: BootGlobal<u8> = BootGlobal(core::cell::UnsafeCell::new(1));

/// Whether `MONITOR`/`MWAIT` is enabled for idle CPUs (`use_monitor`). Written once on the boot CPU
/// in [`allocate_ap_structures`] after `G_MWAIT_MONITOR_ARRAY` and `idle_states` are initialized
/// and before secondary CPUs are started, so it is read-only thereafter.
static USE_MONITOR: BootGlobal<bool> = BootGlobal(core::cell::UnsafeCell::new(false));

struct SyncMwaitMonitorArray(UnsafeCell<MwaitMonitorArray>);

// SAFETY: `G_MWAIT_MONITOR_ARRAY` is initialized once on the boot CPU in `allocate_ap_structures`
// before secondary CPUs are started, and is read-only thereafter.
unsafe impl Sync for SyncMwaitMonitorArray {}

/// Holds an array of `MwaitMonitor` objects used to signal that a CPU is about-to-enter or
/// should-wake-from the idle thread (`gMwaitMonitorArray`).
static G_MWAIT_MONITOR_ARRAY: SyncMwaitMonitorArray =
    SyncMwaitMonitorArray(UnsafeCell::new(MwaitMonitorArray::new()));

/// Fake monitor to use until smp is initialized. The size of the memory range doesn't matter,
/// since it won't actually get used in a non-smp environment (`gFakeMonitor`).
static G_FAKE_MONITOR: MwaitMonitor = MwaitMonitor::new();

/// Target state for [`G_MWAIT_MONITOR_ARRAY`] indicating the CPU is not idle.
const TARGET_STATE_NOT_IDLE: u8 = 0;
/// Target state for [`G_MWAIT_MONITOR_ARRAY`] indicating the CPU is idle.
const TARGET_STATE_IDLE: u8 = 1;

const EMPTY_IDLE_STATE: x86_idle_state_t =
    x86_idle_state_t { name: ptr::null(), mwait_hint: 0, exit_latency: 0, flushes_tlb: false };

/// Also set up a fake table of idle states (`fake_supported_idle_states`).
static FAKE_SUPPORTED_IDLE_STATES: x86_idle_states_t = x86_idle_states_t {
    states: {
        let mut s = [EMPTY_IDLE_STATE; X86_MAX_CSTATES];
        s[0] = x86_cstate_c1(0);
        s
    },
    default_state_mask: X86_IDLE_STATE_MASK_C1_ONLY,
};

// SAFETY: `FAKE_SUPPORTED_IDLE_STATES` contains valid NUL-terminated `'static` C strings.
static FAKE_IDLE_STATES: X86IdleStates = unsafe { X86IdleStates::new(&FAKE_SUPPORTED_IDLE_STATES) };

/// Thread-safe static wrapper around [`X86Percpu`] for the bootstrap processor.
#[repr(transparent)]
pub(super) struct SyncPercpu(UnsafeCell<X86Percpu>);

impl SyncPercpu {
    /// Returns a raw mutable pointer to the inner [`X86Percpu`].
    #[inline(always)]
    pub(super) const fn get(&self) -> *mut X86Percpu {
        self.0.get()
    }
}

// SAFETY: Per-CPU fields are accessed either on the owning CPU (with interrupts/preemption
// disabled) or via atomic/synchronized cross-CPU fields (`monitor`, `halt_interlock`,
// `idle_states`, `apic_id`).
unsafe impl Sync for SyncPercpu {}

/// Pre-initialize the per cpu structure for the boot cpu. Referenced by early boot code prior to
/// being able to initialize via code.
pub(super) static BP_PERCPU: SyncPercpu = SyncPercpu(UnsafeCell::new(X86Percpu {
    // This `direct` pointer will be used to derive `%gs.base` in early initialization by calling
    // `bp_percpu_ptr().expose_provenance()`, retaining the provenance of the `BP_PERCPU` object.
    direct: (&raw const BP_PERCPU).cast_mut().cast::<X86Percpu>(),
    current_thread: ptr::null_mut(),
    // Set in early boot code, copying over the values set in physboot.
    stack_guard: 0,
    kernel_unsafe_sp: 0,
    saved_user_sp: 0,
    in_restricted_mode: 0,
    blocking_disallowed: 0,
    monitor: &raw const G_FAKE_MONITOR,
    halt_interlock: AtomicU32::new(0),
    idle_states: &raw const FAKE_IDLE_STATES,
    // Start with an invalid ID until we know the local APIC is set up.
    apic_id: INVALID_APIC_ID,
    gpf_return_target: 0,
    cpu_num: 0,
    num_spinlocks: 0,
    last_user_aspace: ptr::null_mut(),
    high_level_percpu: ptr::null_mut(),
    _default_tss_align: Align16,
    default_tss: Tss64::new(),
    interrupt_stacks: InterruptStacks {
        nmi: [0; INTERRUPT_STACK_SIZE],
        machine_check: [0; INTERRUPT_STACK_SIZE],
        double_fault: [0; INTERRUPT_STACK_SIZE],
    },
}));

/// Returns a raw pointer to the bootstrap processor's [`X86Percpu`].
#[inline(always)]
fn bp_percpu_ptr() -> *mut X86Percpu {
    BP_PERCPU.get()
}

/// Returns the `(ptr, len)` of the allocated `ap_percpus` slice.
#[inline(always)]
fn ap_percpus_raw() -> (*mut X86Percpu, usize) {
    // SAFETY: `AP_PERCPUS` is initialized during boot before secondary CPUs start and read-only
    // afterwards.
    unsafe { *AP_PERCPUS.0.get() }
}

/// Returns whether `MONITOR`/`MWAIT` idle states are enabled on the system (`use_monitor`).
#[inline(always)]
fn mp_use_monitor() -> bool {
    // SAFETY: USE_MONITOR is initialized during boot before secondary CPUs start and read-only
    // afterwards.
    unsafe { *USE_MONITOR.0.get() }
}

/// Returns the number of x86 CPUs (`x86_num_cpus`).
#[inline(always)]
pub fn mp_num_cpus() -> u8 {
    // SAFETY: X86_NUM_CPUS is initialized during boot before secondary CPUs start and read-only
    // afterwards.
    unsafe { *X86_NUM_CPUS.0.get() }
}

/// Returns a pointer to the calling CPU's `X86Percpu` (`x86_get_percpu()`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the calling CPU.
#[inline(always)]
pub unsafe fn get_percpu() -> *mut X86Percpu {
    // SAFETY: Caller guarantees `%gs.base` is initialized.
    unsafe {
        ptr::with_exposed_provenance_mut(x86_read_gs_offset64::<PERCPU_DIRECT_OFFSET>() as usize)
    }
}

/// Returns a pointer to `cpu_num`'s `X86Percpu` (`x86_percpu_for` / `x86_get_percpu(cpu_num)`).
///
/// # Safety
///
/// The caller must ensure that `cpu_num == 0` or `cpu_num < x86_num_cpus` after
/// [`allocate_ap_structures`] has completed.
#[inline(always)]
pub unsafe fn percpu_for(cpu_num: cpu_num_t) -> *mut X86Percpu {
    if cpu_num == 0 {
        bp_percpu_ptr()
    } else {
        let (ap_ptr, _ap_len) = ap_percpus_raw();
        ap_ptr.wrapping_add((cpu_num - 1) as usize)
    }
}

/// Allocate all of the necessary structures for all of the APs to run.
///
/// # Safety
///
/// The caller must ensure that this function is called at most once on the bootstrap processor
/// during single-threaded SMP setup, and `apic_ids` points to a valid, live allocation of at least
/// `cpu_count` readable `u32` APIC IDs.
pub unsafe fn allocate_ap_structures(apic_ids: *const u32, cpu_count: u8) -> Result<(), Status> {
    let (_existing_ptr, existing_len) = ap_percpus_raw();
    assert!(existing_len == 0);

    debug_assert!(cpu_count >= 1);
    if cpu_count == 0 {
        return Err(Status::INVALID_ARGS);
    }

    let bootstrap_ap = u32::from(apic_local_id());
    debug_assert!(bootstrap_ap == u32::from(apic_bsp_id()));

    // SAFETY: Caller guarantees `apic_ids` has `cpu_count` readable `u32` elements.
    let apic_ids_slice = unsafe { zr::slice_from_raw_parts(apic_ids, cpu_count as usize) };

    if cpu_count > 1 {
        let ap_count = (cpu_count - 1) as usize;
        let layout = Layout::array::<X86Percpu>(ap_count).map_err(|_| Status::NO_MEMORY)?;
        zr::static_assert!(align_of::<X86Percpu>() == MAX_CACHE_LINE);
        // SAFETY: `ap_count >= 1`, so `layout.size() > 0`; all-zero bytes is a valid initial
        // representation of `X86Percpu` (matching C++ `new x86_percpu[ap_count]{}`).
        let alloc = unsafe { kalloc::alloc_zeroed(layout) }.ok_or(Status::NO_MEMORY)?;
        let ap_ptr = alloc.as_ptr().cast::<X86Percpu>();
        let mut alloc_cleanup = zr::defer(|| {
            // SAFETY: layout is valid, ptr is the one allocated.
            unsafe { kalloc::dealloc(alloc.as_ptr().cast::<u8>(), layout) };
        });

        // Initialize AP fields to valid fake states for !use_monitor case.
        for i in 0..ap_count {
            // SAFETY: `i < ap_count`.
            unsafe {
                let ap = ap_ptr.add(i);
                (*ap).monitor = &raw const G_FAKE_MONITOR;
                (*ap).idle_states = &raw const FAKE_IDLE_STATES;
            }
        }

        let use_monitor = feature::x86_feature_test(feature::X86_FEATURE_MON)
            && feature::x86_get_cpuid_leaf(X86CpuidLeafNum::Mon).is_some()
            && !feature::x86_get_microarch_config().expect("microarch config").idle_prefer_hlt;

        // Use temporary local structs for monitor and idle alloc to allow defer cleanup.
        let mut local_monitor_array = MwaitMonitorArray::new();
        let mut idle_alloc: Option<core::ptr::NonNull<u8>> = None;
        let mut idle_layout = Layout::from_size_align(0, 1).unwrap();

        if use_monitor {
            kprintln!("initializing mwait/monitor for idle threads");
            local_monitor_array.init(cpu_count as usize)?;

            let idle_states_size = size_of::<X86IdleStates>().next_multiple_of(MAX_CACHE_LINE);
            let total_idle_size =
                idle_states_size.checked_mul(cpu_count as usize).ok_or(Status::NO_MEMORY)?;
            let idle_align = align_of::<X86IdleStates>().max(MAX_CACHE_LINE);
            idle_layout = Layout::from_size_align(total_idle_size, idle_align)
                .map_err(|_| Status::NO_MEMORY)?;
            // SAFETY: `total_idle_size > 0`.
            idle_alloc = Some(unsafe { kalloc::alloc(idle_layout) }.ok_or(Status::NO_MEMORY)?);
            let idle_states_base = idle_alloc.unwrap().as_ptr();
            let supported_idle_states = x86_get_idle_states();

            // SAFETY: Each `i * idle_states_size` offset is 8-byte aligned and points to
            // `sizeof(X86IdleStates)` bytes within `idle_alloc`.
            unsafe {
                let bp_idle = idle_states_base.cast::<X86IdleStates>();
                X86IdleStates::init_ptr(bp_idle, supported_idle_states);
                for i in 1..(cpu_count as usize) {
                    let ap_idle =
                        idle_states_base.add(i * idle_states_size).cast::<X86IdleStates>();
                    X86IdleStates::init_ptr(ap_idle, supported_idle_states);
                    (*ap_ptr.add(i - 1)).idle_states = ap_idle;
                    (*ap_ptr.add(i - 1)).monitor = local_monitor_array.get_for_cpu(i as cpu_num_t);
                }
            }
        }

        let mut idle_cleanup = zr::defer(|| {
            if let Some(ptr) = idle_alloc {
                // SAFETY: pointer and layout match what was allocated.
                unsafe { kalloc::dealloc(ptr.as_ptr().cast::<u8>(), idle_layout) };
            }
        });

        let mut apic_idx: u32 = 0;
        for &apic_id in apic_ids_slice {
            if apic_id == bootstrap_ap {
                continue;
            }
            debug_assert!(apic_idx != u32::from(cpu_count - 1));
            if apic_idx == u32::from(cpu_count - 1) {
                // Never found bootstrap CPU in apic id list.
                return Err(Status::BAD_STATE);
            }
            // SAFETY: `apic_idx < cpu_count - 1 == ap_len`.
            unsafe {
                let ap = ap_ptr.add(apic_idx as usize);
                (*ap).cpu_num = apic_idx + 1;
                (*ap).apic_id = apic_id;
                // Provenance of `direct` is exposed at arch.rs:339.
                (*ap).direct = ap;
            }
            apic_idx += 1;
        }

        if apic_idx != u32::from(cpu_count - 1) {
            // Bootstrap CPU appeared more than once in apic id list.
            return Err(Status::BAD_STATE);
        }

        alloc_cleanup.cancel();
        idle_cleanup.cancel();

        // Commit all changes atomically with interrupts disabled.
        let _guard = crate::arch_rs::InterruptDisableGuard::new();
        // SAFETY: Single-threaded boot CPU initialization; arrays are read-only thereafter.
        unsafe {
            *AP_PERCPUS.0.get() = (ap_ptr, ap_count);
            if use_monitor {
                let monitor_array = &mut *G_MWAIT_MONITOR_ARRAY.0.get();
                *monitor_array = local_monitor_array;
                (*bp_percpu_ptr()).monitor = monitor_array.get_for_cpu(BOOT_CPU_ID);
                (*bp_percpu_ptr()).idle_states =
                    idle_alloc.unwrap().as_ptr().cast::<X86IdleStates>();
                *USE_MONITOR.0.get() = true;
            }
            *X86_NUM_CPUS.0.get() = cpu_count;
        }
    } else {
        if apic_ids_slice[0] != bootstrap_ap {
            debug_assert_eq!(apic_ids_slice[0], bootstrap_ap);
            return Err(Status::BAD_STATE);
        }
        let _guard = crate::arch_rs::InterruptDisableGuard::new();
        // SAFETY: Single-threaded boot CPU initialization; `X86_NUM_CPUS` is read-only thereafter.
        unsafe { *X86_NUM_CPUS.0.get() = cpu_count };
    }
    Ok(())
}

/// CPUID reader that uses cached boot CPUID leaves 0 and 1 (`arch::BootCpuidIo`) to avoid redundant
/// `cpuid` instructions for vendor and version info during errata workarounds.
struct BootCpuidIo;

impl regio::x86::Cpuid for BootCpuidIo {
    #[inline]
    fn read_raw(&self, leaf: u32, subleaf: u32) -> regio::x86::CpuidRawResult {
        // SAFETY: `gBootCpuid0` and `gBootCpuidFeature` are initialized on CPU 0 before
        // secondary CPUs are brought online, including from `arch_early_handoff`,
        // `x86_feature_early_init_percpu`, and `x86_feature_init`, and are read-only thereafter.
        unsafe {
            match (leaf, subleaf) {
                (0, 0) => {
                    let [eax, ebx, ecx, edx] = *super::mp_ffi::G_BOOT_CPUID_0.0.get();
                    regio::x86::CpuidRawResult { eax, ebx, ecx, edx }
                }
                (1, 0) => {
                    let [eax, ebx, ecx, edx] = *super::mp_ffi::G_BOOT_CPUID_FEATURE.0.get();
                    regio::x86::CpuidRawResult { eax, ebx, ecx, edx }
                }
                _ => regio::x86::DirectCpuid.read_raw(leaf, subleaf),
            }
        }
    }
}

/// This needs to be run very early in the boot process from
/// ArchPostHandoffBootstrap() and as each CPU is brought up.
/// Initializes the current CPU's per-CPU architectural state (`x86_init_percpu`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, interrupts are disabled, `%gs.base` points to `cpu_num`'s
/// [`X86Percpu`], and the first invocation occurs on CPU 0 during single-threaded early boot
/// before secondary CPUs are started.
#[sanitize(safestack = "off")]
pub unsafe fn init_percpu(cpu_num: cpu_num_t) {
    // SAFETY: Caller guarantees `cpu_num` is valid and `%gs.base` is initialized.
    let percpu = unsafe { percpu_for(cpu_num) };
    // SAFETY: `percpu` points to `cpu_num`'s valid `X86Percpu`.
    unsafe {
        debug_assert!((*percpu).cpu_num == cpu_num);
        debug_assert!((*percpu).direct == percpu);

        // Assembly code has already set up %gs.base so that this function's own code can use it
        // implicitly for stack-protector or safe-stack.
        debug_assert!(read_msr(X86_MSR_IA32_GS_BASE) == percpu.addr() as u64);
    }

    // SAFETY: Caller guarantees CPL 0, interrupts disabled, `%gs.base` initialized, and
    // single-threaded early boot on CPU 0 before secondary CPUs start on the first call.
    unsafe {
        // Set the KERNEL_GS_BASE MSR to 0, which is really user space's GS.
        // When we enter user space, this will be populated via a swapgs.
        write_msr(X86_MSR_IA32_KERNEL_GS_BASE, 0);

        feature::early_init_percpu();

        registers::extended_register_init();
        registers::extended_register_enable_feature(ExtendedRegisterFeature::Sse);
        registers::extended_register_enable_feature(ExtendedRegisterFeature::Avx);

        descriptor::gdt_load(descriptor::gdt_get());

        // Disable the LDT so userspace cannot make segment selectors that point to it.  See
        // https://fxbug.dev/42159255
        descriptor::disable_ldt();

        descriptor::initialize_percpu_tss();
        // Setup the post early boot IDT.
        if cpu_num == 0 {
            let idt = &mut *idt::_idt.get();
            idt.setup();
            // Setup alternate stacks to guarantee stack consistency when handling these interrupts.
            idt.set_ist_index(X86_INT_NMI.0, NMI_IST_INDEX);
            idt.set_ist_index(X86_INT_MACHINE_CHECK.0, MCE_IST_INDEX);
            idt.set_ist_index(X86_INT_DOUBLE_FAULT.0, DBF_IST_INDEX);
            idt::idt_load(idt);
        } else {
            // Load the read-only IDT setup on arch initialization.
            idt::idt_load(idt::idt_get_readonly());
        }

        // Hard disable the SYSENTER instruction. Any execution of the instruction with this MSR
        // selecting the null segment will always #GF.
        write_msr(X86_MSR_IA32_SYSENTER_CS, 0);

        // Load the SYSCALL entry point.
        write_msr(X86_MSR_IA32_LSTAR, super::mp_ffi::x86_syscall as *const () as usize as u64);

        // Set the STAR MSR to load the appropriate kernel code selector on syscall
        // and the appropriate user code selector on return.
        // On syscall entry the following are loaded into segment registers:
        //   CS = CODE_64_SELECTOR      (STAR[47:32])
        //   SS = DATA_SELECTOR         (STAR[47:32] + 0x8)
        // On syscall exit:
        //   CS = USER_CODE_64_SELECTOR (STAR[63:48] + 0x16)
        //   SS = USER_DATA_SELECTOR    (STAR[63:48] + 0x8)
        write_msr(
            X86_MSR_IA32_STAR,
            ((USER_CODE_SELECTOR as u64) << 48) | ((CODE_64_SELECTOR as u64) << 32),
        );

        // Set the FMASK register to mask off certain bits in RFLAGS on syscall
        // entry.  See docs/kernel_invariants.md.
        // Disable alignment check/access control (this prevents ring 0 from performing data access
        // to ring 3 if SMAP is available).
        let mask = X86_FLAGS_AC
            | X86_FLAGS_NT // clear nested task
            | X86_FLAGS_IOPL_MASK // set iopl to 0
            | X86_FLAGS_STATUS_MASK; // clear all status flags, interrupt disabled, trap flag
        write_msr(X86_MSR_IA32_FMASK, mask);

        // Apply the same mask to our current flags, to ensure that flags are set to known-good
        // values, because some flags may be inherited by later kernel threads.  We do this just in
        // case any bad values were left behind by firmware or the bootloader.
        x86_restore_flags(x86_save_flags() & !mask);

        // Enable syscall instruction.
        let mut efer_msr = read_msr(X86_MSR_IA32_EFER);
        efer_msr |= X86_EFER_SCE;
        write_msr(X86_MSR_IA32_EFER, efer_msr);

        let mut cr4 = x86_get_cr4();
        // Enable {rd,wr}{fs,gs}base instructions.
        if feature::x86_feature_test(feature::X86_FEATURE_FSGSBASE) {
            cr4 |= X86_CR4_FSGSBASE;
        }
        if feature::x86_feature_test(feature::X86_FEATURE_UMIP) {
            cr4 |= X86_CR4_UMIP;
        }
        x86_set_cr4(cr4);

        // Store the processor number in IA32_TSC_AUX, so RDTSCP/RDP can efficiently get the current
        // CPU from userspace.
        if feature::x86_feature_test(feature::X86_FEATURE_RDTSCP) {
            write_msr(X86_MSR_IA32_TSC_AUX, u64::from(cpu_num));
        }
    }

    match feature::x86_get_vendor() {
        // SAFETY: Running in CPL 0 on an Intel processor after `x86_feature_early_init_percpu`
        // has initialized `X86_FEATURE_HYPERVISOR` and `x86_microarch_config`.
        X86VendorList::X86_VENDOR_INTEL => unsafe { super::intel::x86_intel_init_percpu() },
        // SAFETY: Running in CPL 0 on an AMD processor.
        X86VendorList::X86_VENDOR_AMD => unsafe { super::amd::x86_amd_init_percpu() },
        X86VendorList::X86_VENDOR_UNKNOWN => {}
    }

    libarch::x86::apply_x86_errata_workarounds(
        &BootCpuidIo,
        &libarch::x86::AMD_C0011028,
        &libarch::x86::AMD_C0011029,
        &libarch::x86::AMD_C001102D,
        &libarch::x86::AMD_LOAD_STORE_CONFIGURATION,
    );
}

/// Sets the bootstrap processor's `apic_id` once the local APIC is initialized
/// (`x86_set_local_apic_id`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized on CPU 0.
#[inline(always)]
pub unsafe fn set_local_apic_id(apic_id: u32) {
    // SAFETY: Caller guarantees `%gs.base` is initialized on CPU 0.
    unsafe {
        let percpu = get_percpu();
        debug_assert!((*percpu).cpu_num == 0);
        (*percpu).apic_id = apic_id;
    }
}

/// Returns `cpu_num`'s local APIC ID (`x86_get_percpu(cpu_num).apic_id`).
///
/// # Safety
///
/// The caller must ensure that `cpu_num < x86_num_cpus`.
#[inline(always)]
pub unsafe fn percpu_get_apic_id(cpu_num: cpu_num_t) -> u32 {
    // SAFETY: Caller guarantees `cpu_num < x86_num_cpus`.
    unsafe { (*percpu_for(cpu_num)).apic_id }
}

/// Returns the calling CPU's local APIC ID (`x86_get_percpu()->apic_id`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized on the calling CPU.
#[inline(always)]
pub unsafe fn curr_percpu_get_apic_id() -> u32 {
    // SAFETY: Caller guarantees `%gs.base` is initialized.
    unsafe { (*get_percpu()).apic_id }
}

/// Translates `apic_id` to its logical CPU number, or `-1` if not found
/// (`x86_apic_id_to_cpu_num`).
#[sanitize(safestack = "off")]
pub fn apic_id_to_cpu_num(apic_id: u32) -> c_int {
    // SAFETY: `bp_percpu.apic_id` and `ap_percpus[i].apic_id` are initialized before secondary
    // CPUs are started and read-only thereafter.
    unsafe {
        if (*bp_percpu_ptr()).apic_id == apic_id {
            return (*bp_percpu_ptr()).cpu_num as c_int;
        }

        let (ap_ptr, _num_aps) = ap_percpus_raw();
        let num_aps_to_scan = usize::from(mp_num_cpus().saturating_sub(1));
        if num_aps_to_scan == 0 {
            return -1;
        }

        let mut i = 0;
        while i < num_aps_to_scan {
            let ap = ap_ptr.wrapping_add(i);
            if (*ap).apic_id == apic_id {
                return (*ap).cpu_num as c_int;
            }
            i += 1;
        }
    }
    -1
}

/// Reschedules tasks on the CPUs specified by `mask` (`arch_mp_reschedule`).
pub fn mp_reschedule(mut mask: cpu_mask_t) {
    let num_cpus = u32::from(mp_num_cpus());
    let valid_mask = 1u32.checked_shl(num_cpus).map_or(u32::MAX, |bit| bit - 1);
    debug_assert_eq!(mask & !valid_mask, 0);
    // Upholds memory safety for safe `mp_reschedule` callers by bounds-checking `mask` to
    // valid CPUs.
    mask &= valid_mask;
    if mask == 0 {
        core::hint::cold_path();
        return;
    }

    let use_monitor = mp_use_monitor();
    let mut needs_ipi: cpu_mask_t = if use_monitor { 0 } else { mask };

    if use_monitor {
        while mask != 0 {
            let cpu_id = lowest_cpu_set(mask);
            let cpu_mask = cpu_num_to_mask(cpu_id);
            let percpu = if cpu_id != 0 {
                let (ap_ptr, ap_len) = ap_percpus_raw();
                debug_assert!((cpu_id as usize) <= ap_len && !ap_ptr.is_null());
                ap_ptr.wrapping_add((cpu_id - 1) as usize)
            } else {
                bp_percpu_ptr()
            };

            // When a cpu sees that it is about to start the idle thread, it sets its own monitor
            // flag. When a cpu is rescheduling another cpu, if it sees the monitor flag set, it
            // can clear the flag to wake up the other cpu w/o an IPI. When the other cpu wakes up,
            // the idle thread sees the cleared flag and preempts itself. Both of these operations
            // synchronize via the lock-free `MwaitMonitor` + `x86_monitor` + IPI fallback protocol.
            // SAFETY: `percpu` is a valid CPU's `X86Percpu`, and `(*percpu).monitor` points to a
            // valid `MwaitMonitor`.
            let old_target_state = unsafe { (*(*percpu).monitor).exchange(TARGET_STATE_NOT_IDLE) };
            if old_target_state != TARGET_STATE_IDLE {
                // CPU was not idle.  We'll need to send it an IPI.
                needs_ipi |= cpu_mask;
            }
            mask &= !cpu_mask;
        }
    } else {
        while mask != 0 {
            let cpu_id = lowest_cpu_set(mask);
            let cpu_mask = cpu_num_to_mask(cpu_id);
            let percpu = if cpu_id != 0 {
                let (ap_ptr, ap_len) = ap_percpus_raw();
                debug_assert!((cpu_id as usize) <= ap_len && !ap_ptr.is_null());
                ap_ptr.wrapping_add((cpu_id - 1) as usize)
            } else {
                bp_percpu_ptr()
            };

            // We are attempting to wake the set up CPUs in |mask| and cause them to schedule a new
            // thread. A target CPU spins for a short time before executing halt; before it spins,
            // it sets the |halt_interlock| flag to '1'. Before a target CPU executes the halt
            // instruction, it sets the |halt_interlock| flag to '2' and skips the halt if the flag
            // was cleared while spinning.  Try to clear the |halt_interlock| flag from 1 -> 0. If
            // we do so, we can skip sending an IPI and prevent an unnecessary halt instruction.
            // SAFETY: `percpu` is a valid CPU's `X86Percpu`, and `halt_interlock` is an atomic
            // field safe for concurrent access.
            let did_fast_wakeup = unsafe {
                (*percpu)
                    .halt_interlock
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            };
            if did_fast_wakeup {
                needs_ipi &= !cpu_mask;
            }
            mask &= !cpu_mask;
        }
    }

    if needs_ipi != 0 {
        mp_send_ipi(MpIpiTarget::Mask, needs_ipi, MpIpi::Reschedule);
    }
}

/// Enters the architectural idle state on the calling CPU.
///
/// # Safety
///
/// The caller must ensure that preemption and interrupts are disabled on the calling CPU.
pub unsafe extern "C" fn enter_idle_state() {
    // Preemption and Interrupts *must* be disabled at this point in time.  We are
    // not allowed to re-enable preemption during this method.  We are only
    // allowed to re-enable interrupts at the very last instant before dropping
    // into the `HLT` or `MWAIT` instruction.  This operation (enabling ints and
    // dropping into the wait instruction) must be effectively atomic.  If it is
    // possible for an interrupt to fire and be completely serviced before we make
    // it into the wake instruction, it means that we have a race which could
    // cause us to miss our wakeup IRQ and get our CPU stuck in its low power
    // state.
    //
    // It's critical that the monitor only indicates this CPU is idle when this
    // thread cannot be preempted.  If we are preempted while "showing idle", the
    // signaling CPU may see we're idle, elide the IPI and result in a lost
    // reschedule event.  Prior to exiting the method (and re-enabling preemption)
    // (i.e. we must set the monitor back to "not idle".
    debug_assert!(!crate::kernel::thread::preempt_is_enabled());
    debug_assert!(crate::arch_rs::ints_disabled());

    // Preemption will be disabled for the entire time in this method; we should
    // be able to rely on this pointer and mask being valid for the duration of
    // the method.
    // SAFETY: Preemption and interrupts are disabled, and `%gs.base` is initialized.
    let percpu = unsafe { get_percpu() };
    // SAFETY: `percpu` is valid on this CPU, and fields are initialized.
    let current_thread = unsafe { (*percpu).current_thread };
    // SAFETY: `percpu` is valid on this CPU, and `cpu_num` is initialized.
    let local_reschedule_mask = cpu_num_to_mask(unsafe { (*percpu).cpu_num });

    if mp_use_monitor() {
        let mut rsb_maybe_empty = false;
        // SAFETY: `(*percpu).monitor` and `(*percpu).idle_states` are valid for the lifetime
        // of the CPU.
        let monitor = unsafe { &*(*percpu).monitor };
        // SAFETY: `(*percpu).idle_states` is initialized before SMP/idle entry and lives for
        // `'static`; counters and `last_idle_duration` use interior atomics.
        let idle_states = unsafe { &*(*percpu).idle_states };

        // Indicate that this CPU is entering the idle state.
        monitor.write(TARGET_STATE_IDLE);

        while monitor.read() == TARGET_STATE_IDLE
            // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
            && unsafe { crate::kernel::thread::thread_preempts_pending(current_thread) } == 0
        {
            let next_state = idle_states.pick_idle_state();
            let mwait_hint = next_state.mwait_hint();

            let _trace = crate::ktrace_rs::cpu_begin_scope_cond!(
                LOCAL_KTRACE_ENABLE,
                "kernel:sched",
                "idle",
                "mwait hint" => mwait_hint
            );

            if feature::x86_cpu_vulnerable_to_rsb_underflow() {
                rsb_maybe_empty |= super::intel::x86_intel_idle_state_may_empty_rsb(next_state);
            }

            // SAFETY: CPL 0 and `monitor` points to the `MwaitMonitor` for this CPU.
            unsafe { monitor.prepare_for_wait() };
            if monitor.read() == TARGET_STATE_IDLE
                // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
                && unsafe { crate::kernel::thread::thread_preempts_pending(current_thread) } == 0
            {
                let start = crate::platform_rs::timer::current_mono_time();
                // AMD-SB-1045: Fallback RSB fill on AMD processors vulnerability.
                if feature::x86_cpu_vulnerable_to_rsb_cross_thread() {
                    core::hint::cold_path();
                    x86_ras_fill();
                }

                // Arming the monitor + checking the monitor state + issuing the MWAIT/HLT
                // instruction with interrupts disabled and immediately re-enabling them inside
                // the instruction effectively establishes an atomic check + sleep which cannot
                // be raced by an IPI or other hardware interrupt.
                //
                // SAFETY: Re-enable interrupts and atomically drop into MWAIT, as required by
                // enter_idle_state preconditions. (Interrupts are transiently enabled before
                // returning disabled).
                unsafe {
                    x86_enable_ints_and_mwait(mwait_hint);
                }
                // SAFETY: We have woken up from MWAIT, must restore the disabled interrupt state.
                unsafe {
                    x86_cli();
                }

                let duration = (crate::platform_rs::timer::current_mono_time() - start).0;
                idle_states.record_duration(duration);
                next_state.record_duration(duration);
                next_state.count_entry();
            }
        }

        // Spectre V2: If we enter a deep sleep state, fill the RSB before RET-ing from this
        // function. (CVE-2017-5715, see Intel "Deep Dive: Retpoline: A Branch Target Injection
        // Mitigation").
        if feature::x86_cpu_vulnerable_to_rsb_underflow() && rsb_maybe_empty {
            core::hint::cold_path();
            x86_ras_fill();
        }

        // Make sure we have a local preempt pending before we exit the method and
        // re-enable preemption.  Also, make sure we set our monitor state back to
        // "not idle"
        // (We must manually set preempt_pending here because the thread_reschedule IPI logic
        // skipped setting it since it saw we were idle, i.e.
        // `monitor.read() == TARGET_STATE_IDLE`).
        // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
        unsafe {
            crate::kernel::thread::thread_preempts_pending_add(
                current_thread,
                local_reschedule_mask,
            );
        }
        monitor.write(TARGET_STATE_NOT_IDLE);
    } else {
        let _trace =
            crate::ktrace_rs::cpu_begin_scope_cond!(LOCAL_KTRACE_ENABLE, "kernel:sched", "idle");
        // Set the halt_interlock flag and spin for a little bit, in case a wakeup happens very
        // shortly before we decide to go to sleep. If the halt_interlock flag is changed, another
        // CPU has woken us, avoid the halt instruction.
        const PAUSE_ITERATIONS: i32 = 3000;
        // SAFETY: `percpu` points to the valid `X86Percpu` for this CPU.
        let halt_interlock = unsafe { &(*percpu).halt_interlock };
        halt_interlock.store(1, Ordering::Relaxed); // Relaxed since it's just a hint to wake up.
        let mut i = 0;
        while i < PAUSE_ITERATIONS
            // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
            && unsafe { crate::kernel::thread::thread_preempts_pending(current_thread) } == 0
        {
            libarch::r#yield();
            if halt_interlock.load(Ordering::Relaxed) != 1 {
                break;
            }
            i += 1;
        }
        // Transition 1 -> 2 means we are committed to HLT. If it was already 0, a fast wakeup
        // occurred.
        let no_fast_wakeup =
            halt_interlock.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst).is_ok();
        if no_fast_wakeup
            // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
            && unsafe { crate::kernel::thread::thread_preempts_pending(current_thread) } == 0
        {
            // SAFETY: We are in the idle thread context, disabling interrupts to safely check
            // for preemption before halting.
            unsafe {
                x86_cli();
            }
            // AMD-SB-1045: Fallback RSB fill on AMD processors vulnerability.
            if feature::x86_cpu_vulnerable_to_rsb_cross_thread() {
                core::hint::cold_path();
                x86_ras_fill();
            }
            // SAFETY: `current_thread` is valid for the duration of `enter_idle_state`.
            if unsafe { crate::kernel::thread::thread_preempts_pending(current_thread) } == 0 {
                // SAFETY: Re-enable interrupts and atomically drop into HLT, as required by
                // enter_idle_state preconditions. (Interrupts transiently enabled inside HLT).
                unsafe {
                    x86_enable_ints_and_hlt();
                }
                // SAFETY: We have woken up from HLT, must restore the disabled interrupt state.
                unsafe {
                    x86_cli();
                }
            }
        }
        // Make sure we have a local preempt pending before we exit the method and
        // re-enable preemption.
        // SAFETY: current_thread is valid for the duration of this function.
        unsafe {
            crate::kernel::thread::thread_preempts_pending_add(
                current_thread,
                local_reschedule_mask,
            );
        }
    }
}

/// Sends an inter-processor interrupt `ipi` to `target` CPUs (`arch_mp_send_ipi`).
pub fn mp_send_ipi(target: MpIpiTarget, mask: cpu_mask_t, ipi: MpIpi) {
    let vector = match ipi {
        MpIpi::Generic => X86_INT_IPI_GENERIC.0,
        MpIpi::Reschedule => X86_INT_IPI_RESCHEDULE.0,
        MpIpi::Interrupt => X86_INT_IPI_INTERRUPT.0,
        MpIpi::Halt => X86_INT_IPI_HALT.0,
    };

    match target {
        MpIpiTarget::AllButLocal => {
            apic_send_broadcast_ipi(vector, ApicInterruptDeliveryMode::Fixed);
        }
        MpIpiTarget::All => {
            apic_send_broadcast_self_ipi(vector, ApicInterruptDeliveryMode::Fixed);
        }
        MpIpiTarget::Mask => {
            apic_send_mask_ipi(vector, mask, ApicInterruptDeliveryMode::Fixed);
        }
    }
}

/// Halts the current CPU upon receiving an `X86_INT_IPI_HALT` IPI.
///
/// # Safety
///
/// The caller must ensure that CPL is 0.
#[cold]
#[inline(never)]
pub(super) unsafe fn ipi_halt_handler() -> ! {
    kprintln!("halting cpu {}", crate::arch_rs::curr_cpu_num());

    crate::platform_pc::power::platform_halt_cpu();

    loop {
        // SAFETY: CPL 0; disables interrupts and halts forever.
        unsafe {
            x86_cli();
            x86_hlt();
        }
    }
}

/// Returns the number of the calling CPU.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub fn arch_curr_cpu_num() -> cpu_num_t {
    // SAFETY: `%gs.base` points at the calling CPU's `x86_percpu` from before any C++ or Rust code
    // runs on that CPU (the stack protector and safe-stack depend on it too), and
    // `PERCPU_CPU_NUM_OFFSET` is the 4-byte aligned offset of its `cpu_num` field.
    unsafe { x86_read_gs_offset32::<PERCPU_CPU_NUM_OFFSET>() }
}

/// Returns the calling CPU's flag that tracks whether it is in restricted mode.
///
/// Call with interrupts or preemption disabled so the calling thread cannot migrate across CPUs
/// and observe a stale flag.
///
/// # Safety
///
/// `%gs.base` must already point to the calling CPU's initialized `x86_percpu` (after
/// `post_handoff_bootstrap` / `x86_init_percpu`).
#[inline(always)]
pub unsafe fn arch_get_restricted_flag() -> bool {
    // SAFETY: Caller guarantees `%gs.base` points to the calling CPU's initialized `x86_percpu`,
    // and `PERCPU_IN_RESTRICTED_MODE` is the 4-byte aligned offset of its `in_restricted_mode`
    // field.
    unsafe { x86_read_gs_offset32::<PERCPU_IN_RESTRICTED_MODE>() != 0 }
}

/// Returns the number of spinlocks held on the calling CPU.
// TODO(https://fxbug.dev/581987928): Remove `#[coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#[coverage(off)]
#[inline(always)]
pub fn num_spinlocks_held() -> u32 {
    // SAFETY: The calling CPU's `x86_percpu` must already be installed in `%gs.base`, so this must
    // not be called from the early-boot code that runs before `IA32_GS_BASE` is written
    // (`post_handoff_bootstrap` on the boot CPU, `secondary_entry` on the others).
    // `PERCPU_NUM_SPINLOCKS_OFFSET` is the 4-byte aligned offset of its `num_spinlocks` field
    // (statically asserted above).
    unsafe { x86_read_gs_offset32::<PERCPU_NUM_SPINLOCKS_OFFSET>() }
}

/// Forcibly stops all other CPUs except the current one and the BSP (which is cpu 0).
///
/// # Safety
///
/// Caller must ensure that:
/// 1. The system is on a terminal halt or panic path where forcibly resetting other APs will not
///    violate kernel invariants required by the remaining execution on this CPU.
/// 2. Interrupts are disabled on the calling CPU so that it cannot migrate to another CPU while
///    iterating over APs.
pub unsafe fn x86_force_halt_all_but_local_and_bsp() {
    let num_cpus = u32::from(mp_num_cpus());
    if num_cpus <= 1 {
        return;
    }
    let self_cpu = crate::arch_rs::curr_cpu_num();
    let (ap_ptr, _ap_len) = ap_percpus_raw();
    for i in 1..num_cpus {
        if i == self_cpu {
            continue;
        }
        // SAFETY: `1 <= i < num_cpus`, so `i - 1 < ap_len`.
        let dst_apic_id = unsafe { (*ap_ptr.wrapping_add((i - 1) as usize)).apic_id };
        if dst_apic_id == INVALID_APIC_ID {
            // Uninitialized or failed AP.
            continue;
        }
        apic_send_ipi(0, dst_apic_id, ApicInterruptDeliveryMode::Init);
    }
}
/// Validates whether `cpu_id` can be unplugged (`arch_mp_prep_cpu_unplug`).
pub fn mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    if cpu_id == 0 || cpu_id >= u32::from(mp_num_cpus()) {
        return Err(Status::INVALID_ARGS);
    }
    Ok(())
}

/// Unplugs secondary processor `cpu_id` by sending an INIT IPI (`arch_mp_cpu_unplug`).
///
/// # Safety
///
/// The caller must ensure that `cpu_id` is a valid secondary CPU ready to be reset.
pub unsafe fn mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // We do not allow unplugging the bootstrap processor.
    if cpu_id == 0 || cpu_id >= u32::from(mp_num_cpus()) {
        return Err(Status::INVALID_ARGS);
    }

    let (ap_ptr, _ap_len) = ap_percpus_raw();
    // SAFETY: `1 <= cpu_id < mp_num_cpus()`, so `cpu_id - 1 < ap_len`.
    let dst_apic_id = unsafe { (*ap_ptr.wrapping_add((cpu_id - 1) as usize)).apic_id };
    if dst_apic_id == INVALID_APIC_ID {
        // This is a transient state that can occur during CPU onlining.
        return Err(Status::UNAVAILABLE);
    }

    debug_assert!(dst_apic_id < u32::from(u8::MAX));
    apic_send_ipi(0, dst_apic_id, ApicInterruptDeliveryMode::Init);
    Ok(())
}

/// Hotplugs secondary processor `cpu_id` (`arch_mp_cpu_hotplug`).
///
/// # Safety
///
/// The caller must ensure that SMP has been initialized via `x86_init_smp`.
pub unsafe fn mp_cpu_hotplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    if cpu_id >= u32::from(mp_num_cpus()) {
        return Err(Status::INVALID_ARGS);
    }
    if is_cpu_online(cpu_id) {
        return Err(Status::BAD_STATE);
    }
    debug_assert!(cpu_id != 0);
    if cpu_id == 0 {
        // We shouldn't be able to shutoff the bootstrap CPU, so
        // no reason to be able to bring it back via this route.
        return Err(Status::INVALID_ARGS);
    }

    let (ap_ptr, _ap_len) = ap_percpus_raw();
    // SAFETY: `1 <= cpu_id < mp_num_cpus()`, so `cpu_id - 1 < ap_len`.
    let apic_id = unsafe {
        let percpu = ap_ptr.wrapping_add((cpu_id - 1) as usize);
        debug_assert!((*percpu).apic_id != INVALID_APIC_ID);
        &raw const (*percpu).apic_id
    };
    // SAFETY: `apic_id` points to 1 valid APIC ID.
    unsafe { super::smp::x86_bringup_aps(apic_id, 1) }
}

/// Flushes caches on the calling CPU, signals `flush_done`, and halts forever
/// (`arch_flush_state_and_halt`).
///
/// Used to suspend work on a CPU until it is further shutdown.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, interrupts are disabled, and `flush_done` points to a
/// valid, live (not freed) `MpUnplugEvent`.
pub unsafe fn flush_state_and_halt(flush_done: *mut MpUnplugEvent) -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());

    // SAFETY: `wbinvd` writes back and invalidates caches at CPL 0.
    unsafe {
        wbinvd();
    }

    crate::kernel::thread::preempt_disable();
    // SAFETY: Caller guarantees `flush_done` points to a valid, live `MpUnplugEvent`.
    unsafe {
        crate::kernel::mp::unplug_event_signal(flush_done);
    }
    loop {
        // SAFETY: CPL 0; disables interrupts and halts forever.
        unsafe {
            x86_cli();
            x86_hlt();
        }
    }
}

/// Associates the high-level `percpu` pointer with `cpu_num`'s [`X86Percpu`]
/// (`arch_setup_percpu`).
///
/// # Safety
///
/// The caller must ensure that `cpu_num < x86_num_cpus` and `percpu` points to a valid, live
/// `percpu` instance for the lifetime of the CPU.
pub unsafe fn setup_percpu(cpu_num: cpu_num_t, percpu: *mut c_void) {
    // SAFETY: Caller guarantees `cpu_num < x86_num_cpus`.
    let arch_percpu = unsafe { percpu_for(cpu_num) };
    debug_assert!(!arch_percpu.is_null());
    // SAFETY: `arch_percpu` is non-null and valid.
    unsafe {
        debug_assert!(
            (*arch_percpu).high_level_percpu.is_null()
                || (*arch_percpu).high_level_percpu == percpu
        );
        let _ = percpu.expose_provenance();
        // Ensure prior `PerCpu` initialization is not reordered past publishing `high_level_percpu`
        // with respect to same-CPU NMI handlers under full LTO.
        core::sync::atomic::compiler_fence(Ordering::Release);
        (*arch_percpu).high_level_percpu = percpu;
    }
}

#[cfg(console_enabled)]
fn bp_idle_states() -> &'static X86IdleStates {
    // SAFETY: `bp_percpu.idle_states` is initialized during early boot and lives for `'static`.
    unsafe { &*(*bp_percpu_ptr()).idle_states }
}

#[cfg(console_enabled)]
fn ap_idle_states(idx: usize) -> &'static X86IdleStates {
    let (ap_ptr, ap_len) = ap_percpus_raw();
    assert!(idx < ap_len);
    // SAFETY: `idx` is bounds-checked above and `ap_percpus[idx].idle_states` is initialized during
    // early SMP boot and lives for `'static`.
    unsafe { (*ap_ptr.wrapping_add(idx)).idle_states.as_ref().unwrap() }
}

#[cfg(console_enabled)]
fn reset_idle_counters(idle_states: &X86IdleStates) {
    for state in idle_states.states() {
        state.reset_counters();
    }
}

#[cfg(console_enabled)]
fn report_idlestates(cpu_num: cpu_num_t, idle_states: &X86IdleStates) {
    kprintln!("CPU {cpu_num}:");
    for state in idle_states.states() {
        let entered = state.times_entered();
        let total = state.cumulative_duration();
        let avg_ns = u64::try_from(total).unwrap_or(0).checked_div(entered).unwrap_or(0);
        kprintln!(
            "  {:>4s} (MWAIT {:02X}): {} entries, {} ns avg duration ({} ns total)",
            state.name(),
            state.mwait_hint(),
            entered,
            avg_ns,
            total
        );
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_idlestates(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements,
    // and `argc > 0` and `!argv.is_null()` are verified above.
    let argv = unsafe { zr::slice_from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("Usage: {:s} (printstats | resetstats | setmask)", argv[0].as_str());
        Status::INVALID_ARGS.into_raw()
    };
    'usage: {
        if argv.len() < 2 {
            break 'usage;
        }
        if !mp_use_monitor() {
            kprintln!("{:s} is only supported on systems with X86_FEATURE_MON", argv[0].as_str());
            return Status::NOT_SUPPORTED.into_raw();
        }
        if argv[1].as_str() == "resetstats" {
            reset_idle_counters(bp_idle_states());
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                reset_idle_counters(ap_idle_states((i - 1) as usize));
            }
        } else if argv[1].as_str() == "printstats" {
            report_idlestates(0, bp_idle_states());
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                report_idlestates(i, ap_idle_states((i - 1) as usize));
            }
        } else if argv[1].as_str() == "setmask" {
            if argv.len() < 3 {
                kprintln!("Usage: {:s} setmask $mask", argv[0].as_str());
                return Status::INVALID_ARGS.into_raw();
            }
            bp_idle_states().set_state_mask(argv[2].arg_uint as u32);
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                ap_idle_states((i - 1) as usize).set_state_mask(argv[2].arg_uint as u32);
            }
        } else {
            break 'usage;
        }
        return ZX_OK;
    }

    usage()
}

#[cfg(console_enabled)]
static_command!(
    CMD_IDLESTATES,
    c"idlestates".as_ptr(),
    c"control or report on CPU idle state selection".as_ptr(),
    cmd_idlestates,
    CMD_AVAIL_NORMAL
);
