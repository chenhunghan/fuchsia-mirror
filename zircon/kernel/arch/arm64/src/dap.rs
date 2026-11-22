// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The ARM debug port: finds each CPU's debug and cross-trigger components through the CoreSight
//! ROM tables, and uses them to stop another CPU and read its registers.

use core::ffi::CStr;
#[cfg(console_enabled)]
use core::ffi::{c_int, c_void};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::{null_mut, with_exposed_provenance_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use boot_options::BootOptions;
use coresight::{
    CIDR1, ClassId, DEVAFF, DEVARCH, MINIMUM_COMPONENT_SIZE, RomTable, arm, class_id_to_string,
    get_designer,
};
use debug::{dprintf, ltrace_entry, ltracef};
use init::{LK_INIT_LEVEL_ARCH, LkInitFlags, LkInitLevel};
use kprint::kprint;
#[cfg(console_enabled)]
use libc::stdio::stdout;
use libc::stdio::{FILE, fprintf};
use regio::arm64::{SysReg, spec};
use regio::{MmioBank, MmioPtr, Offset, Ro, RwUnsafe};
use zerocopy::FromZeros;
use zx_status::Status;
#[cfg(console_enabled)]
use zx_types::ZX_OK;

use super::device_memory_barrier;
use super::exceptions::alt_hex_prefix;
use super::feature::MPIDR_EL1_RAW;
use super::mp::{arch_curr_cpu_num, arch_max_num_cpus};
use crate::arch_rs::ints_disabled;
#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
#[cfg(console_enabled)]
use crate::kernel::mp::is_cpu_online;
use crate::kernel::thread::AutoPreemptDisabler;
#[cfg(console_enabled)]
use crate::kernel::thread::{DEFAULT_PRIORITY, create_with_priority};
use crate::kernel::types::{CPU_MASK_ALL, PAddr, cpu_mask_t, cpu_num_t, cpu_num_to_mask};
use crate::platform_rs::timer::{DurationMono, current_mono_time};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
};
use crate::vm::vm_aspace::VmAspace;

const LOCAL_TRACE: u32 = 0;

const MDRAR_EL1: SysReg<spec::MDRAR_EL1, u64> = SysReg::new();
const DBGAUTHSTATUS_EL1: SysReg<spec::DBGAUTHSTATUS_EL1, u64> = SysReg::new();

/// The register state of a CPU read through its DAP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, FromZeros)]
pub struct Arm64DapProcessorState {
    pub r: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub cpsr: u64,
    pub edscr: u32,

    pub esr_el1: u64,
    pub far_el1: u64,
    pub elr_el1: u64,

    pub esr_el2: u64,
    pub far_el2: u64,
    pub elr_el2: u64,
}

/// This struct describes the aperture in which a ROM table resides.
///
/// An aperture is logically part of a CPU cluster. Prior to booting the cluster the table may
/// appear invalid (reads return zero) so it's important to not read an aperture until its cluster
/// has booted. The aperture's `mask` field is the set of CPUs that belong to the cluster of the
/// given aperture. When walking the ROM tables in an aperture be sure to only do so on CPUs in the
/// `mask`.
struct DapAperture {
    base: PAddr,
    size: usize,
    mask: cpu_mask_t,
    /// Relaxed: `arm_dap_init` stores this before the Release store of [`DAP_SOC`] that publishes
    /// it.
    virt: AtomicPtr<u32>,
}

/// One CPU's debug and cross-trigger components. Each CPU fills in its own, in
/// `arm_dap_init_percpu`, while any CPU can read it.
#[derive(FromZeros)]
struct DebugPort {
    /// Release on store and Acquire on load in [`enabled_daps`], to publish `dap` and `cti`.
    initialized: AtomicBool,
    /// Relaxed: only ever printed.
    cpu_num: AtomicU32,
    /// pointer to the DAP register window
    ///
    /// Relaxed: `initialized` publishes this.
    dap: AtomicPtr<u32>,
    /// pointer to the CTI register window
    ///
    /// Relaxed: `initialized` publishes this.
    cti: AtomicPtr<u32>,
    /// Set while `arm64_dap_read_processor_state` drives this CPU's debug registers.
    ///
    /// Acquire on the swap that sets it and Release on the store that clears it, so that one
    /// read's register accesses end before the next one's start.
    reading: AtomicBool,
}

// Pre-canned values for the debug rom table base, taken from
// the manuals for these particular SoCs.
// TODO(https://fxbug.dev/587037925): read the apertures from the ZBI as well

/// The debug apertures of the T931G and the A311D.
static T931G_APERTURE: [DapAperture; 2] = [
    DapAperture {
        base: PAddr(0xf580_0000), // A53_BASE
        size: 0x80_0000,
        mask: cpu_num_to_mask(0) | cpu_num_to_mask(1),
        virt: AtomicPtr::new(null_mut()),
    },
    DapAperture {
        base: PAddr(0xf500_0000), // A73_BASE
        size: 0x80_0000,
        mask: cpu_num_to_mask(2) | cpu_num_to_mask(3) | cpu_num_to_mask(4) | cpu_num_to_mask(5),
        virt: AtomicPtr::new(null_mut()),
    },
];
/// The debug aperture of the S905D2 and the S905D3G.
static S905_APERTURE: [DapAperture; 1] = [DapAperture {
    base: PAddr(0xf580_0000), // A53_BASE
    size: 0x80_0000,
    mask: CPU_MASK_ALL,
    virt: AtomicPtr::new(null_mut()),
}];

// `DAP_SOC` and `DAPS` are atomics rather than `LazyInit`, which has no unset state to read:
// unless kernel.arm64.debug.dap-rom-soc names a SoC, `arm_dap_init` leaves them unset, and
// `arm64_dap_is_enabled` must then answer false.

/// Which apertures `arm_dap_init` chose, as one of the `SOC_*` values.
///
/// Release on store and Acquire on load in [`dap_apertures`], to publish [`DAPS`] and the
/// apertures' `virt`.
static DAP_SOC: AtomicU8 = AtomicU8::new(SOC_NONE);
/// [`DAP_SOC`] when there are no apertures.
const SOC_NONE: u8 = 0;
/// [`DAP_SOC`] for [`T931G_APERTURE`].
const SOC_T931G: u8 = 1;
/// [`DAP_SOC`] for [`S905_APERTURE`].
const SOC_S905: u8 = 2;

/// A heap array with one entry per CPU, never freed.
///
/// Release on store and Acquire on load in [`daps`], to publish [`DAPS_COUNT`] and the zeroed
/// array.
static DAPS: AtomicPtr<DebugPort> = AtomicPtr::new(null_mut());
/// The length of [`DAPS`].
///
/// Relaxed: [`DAPS`] publishes it.
static DAPS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Returns the apertures that `soc`, a [`DAP_SOC`] value, names.
fn soc_apertures(soc: u8) -> &'static [DapAperture] {
    match soc {
        SOC_T931G => &T931G_APERTURE,
        SOC_S905 => &S905_APERTURE,
        _ => &[],
    }
}

/// Returns the apertures `arm_dap_init` chose, or none before it has tried to map them. One it
/// did not map has a null `virt`.
fn dap_apertures() -> &'static [DapAperture] {
    soc_apertures(DAP_SOC.load(Ordering::Acquire))
}

/// Returns the per-CPU ports `arm_dap_init` allocated, or none before it does or if that failed.
fn daps() -> &'static [DebugPort] {
    let daps = DAPS.load(Ordering::Acquire);
    if daps.is_null() {
        return &[];
    }
    let count = DAPS_COUNT.load(Ordering::Relaxed);
    // SAFETY: `arm_dap_init` runs its body once, and stores `DAPS_COUNT` before the Release store
    // of `DAPS`, so this Acquire load of a non-null `DAPS` sees the count stored with it. The pair
    // describes a zeroed heap array that is never freed, and every field of `DebugPort` is atomic.
    unsafe { core::slice::from_raw_parts(daps, count) }
}

/// Called on the boot CPU.
fn arm_dap_init(_level: LkInitLevel) {
    ltrace_entry!();

    /// Set by the first call, the only one that does anything.
    static STARTED: AtomicBool = AtomicBool::new(false);
    // Relaxed: this only picks the one call that runs. `DAPS` and `DAP_SOC` publish its stores.
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    // Parse the valid options out of the kernel command line
    // kernel.arm64.debug.dap-rom-soc
    let dap_rom_soc =
        CStr::from_bytes_until_nul(&BootOptions::get().arm64_debug_dap_rom_soc).unwrap_or(c"");
    let dap_rom_soc = dap_rom_soc.to_bytes();
    // Set the ROM table locations to search in for DAP apertures for each cpu
    let soc = match dap_rom_soc {
        // two dap register windows for t931g and a311d
        b"amlogic-t931g" | b"amlogic-a311d" => SOC_T931G,
        // s905d2 and s905d3g have the same aperture
        b"amlogic-s905d2" | b"amlogic-s905d3g" => SOC_S905,
        b"" => return,
        _ => {
            dprintf!(
                INFO,
                "ARM DAP: unrecognized non-empty option passed '{}'\n",
                dap_rom_soc.escape_ascii()
            );
            return;
        }
    };

    // allocate a list of parsed dap structures, to be filled in by each cpu as they run their init
    // hook
    {
        let Ok(dp) = kalloc::Box::<[DebugPort]>::try_new_zeroed_slice(arch_max_num_cpus() as usize)
        else {
            return;
        };
        let dp = kalloc::Box::into_raw(dp);
        DAPS_COUNT.store(dp.len(), Ordering::Relaxed);
        DAPS.store(dp.cast(), Ordering::Release);
    }

    dprintf!(INFO, "DAP: enabling dap for {}\n", dap_rom_soc.escape_ascii());

    // map the dap base into the kernel
    for da in soc_apertures(soc) {
        ltracef!(
            "mapping aperture: base {:#x} size {:#x} mask {:#x}\n",
            da.base.0,
            da.size,
            da.mask
        );

        // SAFETY: The boot option names the SoC, and `da` is that SoC's debug aperture from its
        // manual. The range holds device registers, so uncached device memory is the right cache
        // policy for every mapping of it, and mapping it read-write aliases no kernel memory.
        let result = unsafe {
            VmAspace::kernel_aspace().alloc_physical(
                b"arm dap",
                da.size,           // size
                None,              // requested virtual address
                page::SHIFT as u8, // alignment log2
                da.base,           // physical address
                0,                 // vmm flags
                // arch mmu flags
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_UNCACHED_DEVICE,
            )
        };
        let Ok(vaddr) = result else {
            kprint!("failed to map dap address\n");
            // The apertures are still published below: the per-CPU walk skips an unmapped one.
            break;
        };
        let virt = with_exposed_provenance_mut::<u32>(vaddr);
        da.virt.store(virt, Ordering::Relaxed);

        ltracef!("dap address {:p}\n", virt);
    }

    DAP_SOC.store(soc, Ordering::Release);
}

init::lk_init_hook!(arm_dap, arm_dap_init, LK_INIT_LEVEL_ARCH);

/// per cpu walk the dap rom tables, looking for debug components associated with this cpu
fn arm_dap_init_percpu(_level: LkInitLevel) {
    ltrace_entry!();

    let curr_cpu_num = arch_curr_cpu_num();

    ltracef!("cpu-{} mdrar {:#x}\n", curr_cpu_num, MDRAR_EL1.read());
    ltracef!("cpu-{} dbgauthstatus {:#x}\n", curr_cpu_num, DBGAUTHSTATUS_EL1.read());

    let apertures = dap_apertures();
    if apertures.is_empty() {
        ltracef!("cpu-{} no apertures\n", curr_cpu_num);
        return;
    }

    let curr_cpu_mpidr = MPIDR_EL1_RAW.read();
    // `arm_dap_init` publishes the apertures only after it allocates the ports.
    let dp = &daps()[curr_cpu_num as usize];

    for da in apertures {
        let virt = da.virt.load(Ordering::Relaxed);
        if virt.is_null() {
            ltracef!("cpu-{} not mapped, skipping aperture paddr {:#x}\n", curr_cpu_num, da.base.0);
            continue;
        }

        // Is the aperture associated with this CPU?
        if (cpu_num_to_mask(curr_cpu_num) & da.mask) == 0 {
            // Nope, skip it.
            ltracef!(
                "cpu-{} not in mask, skipping aperture paddr {:#x}\n",
                curr_cpu_num,
                da.base.0
            );
            continue;
        }

        ltracef!(
            "cpu-{} walking ROM table at paddr {:#x}, vaddr {:p}\n",
            curr_cpu_num,
            da.base.0,
            virt
        );

        let Ok(max_offset) = u32::try_from(da.size) else {
            kprint!("DAP: aperture at {:#x} is too large to walk\n", da.base.0);
            continue;
        };

        // Walk the ROM table to find the debug interface for this CPU.
        // SAFETY: `arm_dap_init` mapped `da.size` page-aligned bytes of device memory at `virt` and
        // never unmaps them, and this CPU is in the aperture's cluster, so the cluster has booted.
        let mmio = unsafe { MmioPtr::<u32, Ro>::new(virt) };
        let callback = |offset: u32| {
            let component = virt.wrapping_byte_add(offset as usize);
            let mmio = MmioBank::new(
                // SAFETY: `RomTable::walk` passes only multiples of `MINIMUM_COMPONENT_SIZE` that
                // leave the component's first `MINIMUM_COMPONENT_SIZE` bytes inside the mapping
                // above, so `component` is aligned and its first page is mapped.
                unsafe { MmioPtr::<u32, Ro>::new(component) },
                MINIMUM_COMPONENT_SIZE,
            );
            // SAFETY: `at` panics unless the register is aligned and in the component's first
            // `MINIMUM_COMPONENT_SIZE` bytes, which are inside the mapping as above.
            let classid = unsafe { mmio.at(CIDR1) }.read().try_classid();

            // We're only interested in ARM-architected CoreSight components.
            // SAFETY: As for `CIDR1`.
            let arch_reg = unsafe { mmio.at(DEVARCH) }.read();
            let architect = if arch_reg.architect() != 0 {
                arch_reg.architect()
            } else {
                // SAFETY: `mmio` is the component's first `MINIMUM_COMPONENT_SIZE` bytes, which
                // are inside the mapping as above.
                unsafe { get_designer(&mmio) }
            };
            if architect != arm::ARCHITECT {
                ltracef!(
                    "cpu-{} ignoring component with architect {:#x}\n",
                    curr_cpu_num,
                    architect
                );
                return;
            }
            if !matches!(classid, Ok(ClassId::CoreSight)) {
                ltracef!(
                    "cpu-{} ignoring component with classid {}\n",
                    curr_cpu_num,
                    classid.map_or("unknown", class_id_to_string)
                );
                return;
            }

            // We're only interested in components for *this* CPU.
            // DEVAFF0 and DEVAFF1 are the low and high halves of the affinity, read as 32-bit
            // registers like the others here.
            // SAFETY: As for `CIDR1`.
            let devaff0 = unsafe { mmio.at(Offset::<u32, Ro>::new(DEVAFF.value)) }.read();
            // SAFETY: As for `CIDR1`.
            let devaff1 = unsafe { mmio.at(Offset::<u32, Ro>::new(DEVAFF.value + 4)) }.read();
            let component_affinity = (u64::from(devaff1) << 32) | u64::from(devaff0);
            if component_affinity != curr_cpu_mpidr {
                ltracef!(
                    "cpu-{} ignoring component with affinity {:#x}\n",
                    curr_cpu_num,
                    component_affinity
                );
                return;
            }

            // We're only interested in Core Debug Interface and ARM Cross-Trigger Matrix
            // components.
            let archid = arch_reg.archid();
            match archid {
                arm::archid::CTI => {
                    dp.cpu_num.store(curr_cpu_num, Ordering::Relaxed);
                    dp.cti.store(component, Ordering::Relaxed);
                }
                arm::archid::CORE_DEBUG_INTERFACE_8_0A
                | arm::archid::CORE_DEBUG_INTERFACE_8_1A
                | arm::archid::CORE_DEBUG_INTERFACE_8_2A => {
                    dp.cpu_num.store(curr_cpu_num, Ordering::Relaxed);
                    dp.dap.store(component, Ordering::Relaxed);
                }

                _ => {
                    ltracef!("ignoring component with archid {:#x}\n", archid);
                }
            }
            // Relaxed: only this CPU stores these.
            if !dp.cti.load(Ordering::Relaxed).is_null()
                && !dp.dap.load(Ordering::Relaxed).is_null()
            {
                // Release: publishes `dap` and `cti` to the Acquire load in `enabled_daps`.
                dp.initialized.store(true, Ordering::Release);
            }
        };
        // SAFETY: `mmio` is the page-aligned start of the `da.size` bytes mapped at `virt` above.
        let result = unsafe { RomTable::walk(mmio, max_offset, callback) };
        if let Err(error) = result {
            // Walk offsets wrap at 32 bits, so one below the base table is a negative `i32`.
            let addr = da.base.0.wrapping_add_signed(error.offset as i32 as isize);
            kprint!(
                concat!(
                    "DAP: error during ROM table walk (base {:cs}{:x}) at address {:cs}{:x} on ",
                    "cpu-{:u}: {:s}\n"
                ),
                alt_hex_prefix(da.base),
                da.base.0,
                alt_hex_prefix(addr as u64),
                addr,
                curr_cpu_num,
                error.reason
            );
        }
    }

    // Relaxed: this CPU either made the store or made it before it was last unplugged.
    if !dp.initialized.load(Ordering::Relaxed) {
        kprint!("DAP: failed to find components for cpu-{:u}\n", curr_cpu_num);
    }
}

init::lk_init_hook_flags!(
    arm_dap_percpu,
    arm_dap_init_percpu,
    LkInitLevel(LK_INIT_LEVEL_ARCH.0 + 1),
    LkInitFlags::AllCpus
);

/// How long `RegBlock::wait_for` polls a register.
const WAIT_FOR_TIMEOUT: DurationMono = DurationMono::from_millis(250);

/// helper type to access registers within a memory block
struct RegBlock<T> {
    regs: MmioBank<u32, RwUnsafe>,
    _reg_type: PhantomData<T>,
}

impl<T: Copy + Into<usize>> RegBlock<T> {
    /// # Safety
    ///
    /// `regs` must be the 4-byte aligned address of a mapped register window of the kind `T` names,
    /// `MINIMUM_COMPONENT_SIZE` bytes long, which stays mapped, and in which the registers `T`
    /// names can be read and written without trapping, for the life of the returned value.
    unsafe fn new(regs: *mut u32) -> Self {
        debug_assert!(regs.is_aligned());
        // SAFETY: The caller's guarantee.
        let regs = unsafe { MmioPtr::<u32, RwUnsafe>::new(regs) };
        Self { regs: MmioBank::new(regs, MINIMUM_COMPONENT_SIZE), _reg_type: PhantomData }
    }

    /// # Safety
    ///
    /// The caller must ensure that the effect of writing `val` to `reg_offset`, as listed below,
    /// is sound at this point. EDLAR and CTILAR take an unlock key, and EDRCR clears sticky bits.
    /// The other CTI registers route, raise and acknowledge events, which can halt the victim CPU,
    /// and nothing in the kernel restarts it. A write to EDITR runs `val` as an instruction on the
    /// victim, so `val` must meet [`run_instruction`]'s `# Safety`.
    unsafe fn write(&self, reg_offset: T, val: u32) {
        // SAFETY: `at` panics unless the offset is aligned and in the window that `new`'s caller
        // vouched for, and the caller answers for what the write does.
        unsafe { self.regs.at(Offset::<u32, RwUnsafe>::new(reg_offset.into())).write(val) };
        device_memory_barrier();
    }

    fn read(&self, reg_offset: T) -> u32 {
        // SAFETY: `at` panics unless the offset is aligned and in the window that `new`'s caller
        // vouched for.
        unsafe { self.regs.at(Offset::<u32, RwUnsafe>::new(reg_offset.into())) }.read()
    }

    /// Out of line so that every poll shares one frame: inlined into the register fetches, it grows
    /// the frame that `arm64_dap_read_processor_state` is inlined into, which the lockup path holds
    /// across this `printf`.
    #[inline(never)]
    fn wait_for(&self, reg_offset: T, mask: u32, val: u32) -> Result<(), Status> {
        let reg_offset: usize = reg_offset.into();
        // SAFETY: `at` panics unless the offset is aligned and in the window that `new`'s caller
        // vouched for.
        let reg = unsafe { self.regs.at(Offset::<u32, RwUnsafe>::new(reg_offset)) };
        let t = current_mono_time();

        loop {
            let temp = reg.read();
            if (temp & mask) == val {
                return Ok(());
            }
            if current_mono_time() - t >= WAIT_FOR_TIMEOUT {
                // One `printf` rather than `tracef!`: `core::fmt` nests too deep for the lockup
                // path.
                kprint!(
                    "{:s}:{:u}: timed out, reg {:#x} val {:#x}, mask {:#x} want {:#x}\n",
                    core::module_path!(),
                    core::line!(),
                    reg_offset,
                    temp,
                    mask,
                    val
                );
                return Err(Status::TIMED_OUT);
            }
        }
    }
}

/// CTI register offsets.
#[allow(clippy::upper_case_acronyms)]
#[repr(usize)]
#[derive(Clone, Copy)]
enum CtiRegs {
    CTICONTROL = 0x0,
    CTIINTACK = 0x10,
    CTIAPPPULSE = 0x1c,
    CTIOUTEN0 = 0xa0,
    CTIOUTEN1 = 0xa4,
    CTIGATE = 0x140,
    CTILAR = 0xfb0,
    CTILSR = 0xfb4,
}

impl From<CtiRegs> for usize {
    fn from(reg: CtiRegs) -> usize {
        reg as usize
    }
}

/// External debug register offsets in the core power domain. These return an error response
/// while the core is powered down, OS-locked or double-locked.
#[allow(clippy::upper_case_acronyms)]
#[repr(usize)]
#[derive(Clone, Copy)]
enum DapCoreRegs {
    DBGDTRRX = 0x80,
    EDITR = 0x84,
    EDSCR = 0x88,
    DBGDTRTX = 0x8c,
    EDRCR = 0x90,
}

impl From<DapCoreRegs> for usize {
    fn from(reg: DapCoreRegs) -> usize {
        reg as usize
    }
}

/// External debug register offsets in the debug power domain, or in the core power domain with
/// FEAT_DoPD. These return an error response only while the core is powered down, and only with
/// FEAT_DoPD.
#[allow(clippy::upper_case_acronyms)]
#[repr(usize)]
#[derive(Clone, Copy)]
enum DapDebugRegs {
    EDPRSR = 0x314,
    EDLAR = 0xfb0,
    EDLSR = 0xfb4,
    DBGAUTHSTATUS = 0xfb8,
}

impl From<DapDebugRegs> for usize {
    fn from(reg: DapDebugRegs) -> usize {
        reg as usize
    }
}

/// The key that EDLAR and CTILAR take to unlock the other registers of their component.
const CORESIGHT_UNLOCK_KEY: u32 = 0xc5ac_ce55;

/// CTICONTROL.GLBEN: enables the CTI.
const CTICONTROL_GLBEN: u32 = 1 << 0;
/// Channel 0's bit in CTIAPPPULSE and the CTIOUTEN registers.
const CTI_CHANNEL_0: u32 = 1 << 0;
/// Output trigger 0's bit in CTIINTACK. That trigger is the victim's debug request.
const CTI_TRIGGER_DEBUG_REQUEST: u32 = 1 << 0;

/// EDRCR.CSPA: clears the sticky EDSCR.PipeAdv bit.
const EDRCR_CSPA: u32 = 1 << 3;
/// EDSCR.ITE: EDITR is empty, ready for an instruction.
const EDSCR_ITE: u32 = 1 << 24;
/// EDSCR.PipeAdv: the victim's pipeline has advanced.
const EDSCR_PIPEADV: u32 = 1 << 25;
/// EDSCR.TXfull: DBGDTRTX holds a value for the debugger.
const EDSCR_TXFULL: u32 = 1 << 29;
/// EDPRSR.PU: the victim's core power domain is on.
const EDPRSR_PU: u32 = 1 << 0;
/// EDPRSR.R: the victim's non-debug logic is in reset.
const EDPRSR_R: u32 = 1 << 2;
/// EDPRSR.HALTED: the victim is in Debug state.
const EDPRSR_HALTED: u32 = 1 << 4;
/// EDPRSR.OSLK: the OS lock is set, which blocks access to the core-domain registers.
const EDPRSR_OSLK: u32 = 1 << 5;
/// EDPRSR.DLK: the OS double lock is set, which blocks access to the core-domain registers.
const EDPRSR_DLK: u32 = 1 << 6;

// some pre-canned arm instructions
/// nop
const ARM64_NOP: u32 = 0xd503201f;
/// msr dbgdtr_el0, x0 -- write x0 to dbgdtr
const ARM64_MSR_DBGDTR: u32 = 0xd5130400;
zr::static_assert!(
    ARM64_MSR_DBGDTR & 0x1f == 0,
    "read_processor_state ORs a register number into Rt, bits [4:0], of ARM64_MSR_DBGDTR"
);
/// mov x0, sp
const ARM64_MOV_SP: u32 = 0x910003e0;
/// mrs x0, dlr_el0    -- write dlr to x0
const ARM64_MRS_DLR: u32 = 0xd53b4520;
/// mrs x0, dspsr_el0  -- write dspsr to x0
const ARM64_MRS_DSPSR: u32 = 0xd53b4500;
/// mrs x0, esr_el1    -- write esr_el1 to x0
const ARM64_MRS_ESR_EL1: u32 = 0xd5385200;
/// mrs x0, esr_el2    -- write esr_el2 to x0
const ARM64_MRS_ESR_EL2: u32 = 0xd53c5200;
/// mrs x0, far_el1    -- write far_el1 to x0
const ARM64_MRS_FAR_EL1: u32 = 0xd5386000;
/// mrs x0, far_el2    -- write far_el2 to x0
const ARM64_MRS_FAR_EL2: u32 = 0xd53c6000;
/// mrs x0, elr_el1    -- write elr_el1 to x0
const ARM64_MRS_ELR_EL1: u32 = 0xd5384020;
/// mrs x0, elr_el2    -- write elr_el2 to x0
const ARM64_MRS_ELR_EL2: u32 = 0xd53c4020;

/// Runs `instruction` on the victim CPU, by writing it to EDITR.
///
/// Always inlined, like the other helpers of `arm64_dap_read_processor_state`, so that no frame
/// sits between it and `RegBlock::wait_for` on the lockup path.
///
/// # Safety
///
/// `instruction` must be one of the `ARM64_*` constants, with a register number for
/// `ARM64_MSR_DBGDTR`. `ARM64_NOP` does nothing, and the others copy a register into `x0` or
/// `dbgdtr_el0`. None of them writes memory.
///
/// EDPRSR must have reported `dap`'s CPU, the victim, halted, and the victim must not have
/// restarted since. The `x0` these overwrite then belongs to a CPU that does not resume, because
/// nothing in the kernel restarts a halted CPU.
#[inline(always)]
unsafe fn run_instruction(dap: &RegBlock<DapCoreRegs>, instruction: u32) -> Result<(), Status> {
    ltracef!("DAP: running instruction {:#x}\n", instruction);
    // SAFETY: This EDRCR bit only clears the sticky EDSCR.PipeAdv bit.
    unsafe { dap.write(DapCoreRegs::EDRCR, EDRCR_CSPA) }; // clear the EDSCR.PipeAdv bit

    // wait for EDSCR.PipeAdv == 0 and EDSCR.ITE == 1
    dap.wait_for(DapCoreRegs::EDSCR, EDSCR_PIPEADV | EDSCR_ITE, EDSCR_ITE)?;

    // write the instruction
    // SAFETY: The caller's guarantee covers `instruction` and the victim's state.
    unsafe { dap.write(DapCoreRegs::EDITR, instruction) };

    // wait for EDSCR.PipeAdv == 1 and EDSCR.ITE == 1
    // TODO(https://fxbug.dev/587037925): figure out why pipeadv doesn't always set
    // dap.wait_for(DapCoreRegs::EDSCR, EDSCR_PIPEADV | EDSCR_ITE, EDSCR_PIPEADV | EDSCR_ITE)?;

    ltracef!("DAP: done running instruction {:#x}\n", instruction);
    Ok(())
}

#[inline(always)]
fn read_dcc(dap: &RegBlock<DapCoreRegs>) -> Result<u64, Status> {
    dap.wait_for(DapCoreRegs::EDSCR, EDSCR_TXFULL, EDSCR_TXFULL)?; // wait for TXFull

    Ok((u64::from(dap.read(DapCoreRegs::DBGDTRRX)) << 32)
        | u64::from(dap.read(DapCoreRegs::DBGDTRTX)))
}

/// Fetch a register from the target processor.
///
/// We do this by executing on the remote processor a given instruction that
/// is expected to write the target register into x0. This register is then
/// written to DBGDTR on the remote processor, and then read locally.
///
/// # Safety
///
/// `reg_read_instruction` and the victim must meet [`run_instruction`]'s `# Safety`.
#[inline(always)]
unsafe fn fetch_remote_register(
    dap: &RegBlock<DapCoreRegs>,
    reg_read_instruction: u32,
) -> Result<u64, Status> {
    // Fetch register to x0.
    // SAFETY: The caller's guarantee.
    unsafe { run_instruction(dap, reg_read_instruction) }?;

    // Write to DBGDTR.
    // SAFETY: `ARM64_MSR_DBGDTR` copies `x0` into `dbgdtr_el0`, and the caller guarantees the
    // victim is halted.
    unsafe { run_instruction(dap, ARM64_MSR_DBGDTR) }?;

    // Read the result.
    read_dcc(dap)
}

/// Zeroes all of `state`, then fills it in place, so `state` is initialized whenever this returns.
/// At 328 bytes it does not belong in a frame on this path.
///
/// Always inlined into `arm64_dap_read_processor_state`, its only caller, so that no frame sits
/// between that and `RegBlock::wait_for` on the lockup path.
///
/// # Safety
///
/// EDPRSR must have reported `dap`'s CPU, the victim, halted, and the victim must not have
/// restarted since. [`run_instruction`] needs that, and this function supplies the instructions
/// itself.
#[inline(always)]
unsafe fn read_processor_state(
    dap: &RegBlock<DapCoreRegs>,
    state: &mut MaybeUninit<Arm64DapProcessorState>,
) -> Result<(), Status> {
    // Clear out state.
    // SAFETY: `state` is writable for one `Arm64DapProcessorState`, zeroed in place.
    unsafe { core::ptr::write_bytes(state.as_mut_ptr(), 0, 1) };
    // SAFETY: `Arm64DapProcessorState` is `FromZeros`, so the zero bytes just written are a valid
    // value.
    let state = unsafe { state.assume_init_mut() };

    // save a copy of the EDSCR which has EL level and other things
    state.edscr = dap.read(DapCoreRegs::EDSCR);
    let el_level = state.get_el_level();

    // read x0 - x30
    // mov xN -> dbgdtr, read out of our end of the DCC
    for i in 0..=30u32 {
        // SAFETY: `ARM64_MSR_DBGDTR`'s Rt field is zero, as asserted at its definition, so with
        // `i` at most 30, `ARM64_MSR_DBGDTR | i` copies `xi` into `dbgdtr_el0`. The caller
        // guarantees the halt.
        unsafe { run_instruction(dap, ARM64_MSR_DBGDTR | i) }?;

        state.r[i as usize] = read_dcc(dap)?;
    }

    // Read the PC (saved in the DLR_EL0 register), SP, and CPSR (saved in DSPSR_EL0).
    // SAFETY: `ARM64_MRS_DLR` copies `dlr_el0` into `x0`. The caller guarantees the halt.
    state.pc = unsafe { fetch_remote_register(dap, ARM64_MRS_DLR) }?;
    // SAFETY: `ARM64_MOV_SP` copies `sp` into `x0`. The caller guarantees the halt.
    state.sp = unsafe { fetch_remote_register(dap, ARM64_MOV_SP) }?;
    // SAFETY: `ARM64_MRS_DSPSR` copies `dspsr_el0` into `x0`. The caller guarantees the halt.
    state.cpsr = unsafe { fetch_remote_register(dap, ARM64_MRS_DSPSR) }?;

    // If running in EL1 or above, fetch EL1 exception state.
    if el_level >= 1 {
        // SAFETY: `ARM64_MRS_ESR_EL1` copies `esr_el1` into `x0`. The caller guarantees the halt.
        state.esr_el1 = unsafe { fetch_remote_register(dap, ARM64_MRS_ESR_EL1) }?;
        // SAFETY: `ARM64_MRS_FAR_EL1` copies `far_el1` into `x0`. The caller guarantees the halt.
        state.far_el1 = unsafe { fetch_remote_register(dap, ARM64_MRS_FAR_EL1) }?;
        // SAFETY: `ARM64_MRS_ELR_EL1` copies `elr_el1` into `x0`. The caller guarantees the halt.
        state.elr_el1 = unsafe { fetch_remote_register(dap, ARM64_MRS_ELR_EL1) }?;
    }

    // If running in EL2 or above, fetch EL2 exception state.
    if el_level >= 2 {
        // SAFETY: `ARM64_MRS_ESR_EL2` copies `esr_el2` into `x0`. The caller guarantees the halt.
        state.esr_el2 = unsafe { fetch_remote_register(dap, ARM64_MRS_ESR_EL2) }?;
        // SAFETY: `ARM64_MRS_FAR_EL2` copies `far_el2` into `x0`. The caller guarantees the halt.
        state.far_el2 = unsafe { fetch_remote_register(dap, ARM64_MRS_FAR_EL2) }?;
        // SAFETY: `ARM64_MRS_ELR_EL2` copies `elr_el2` into `x0`. The caller guarantees the halt.
        state.elr_el2 = unsafe { fetch_remote_register(dap, ARM64_MRS_ELR_EL2) }?;
    }

    // TODO(https://fxbug.dev/587037925): put x0 back so the cpu could be restarted

    Ok(())
}

/// Starts a thread on CPU 0 that halts `cpu` and prints its registers.
///
/// # Safety
///
/// `cpu` must stay powered up until the `dap debug` thread this starts has finished its read.
#[cfg(console_enabled)]
unsafe fn cpu_debug_command(cpu: cpu_num_t) {
    kprint!("attempting to debug cpu {:u}\n", cpu);

    if cpu == 0 || cpu >= arch_max_num_cpus() {
        kprint!("invalid cpu, cannot be 0 or out of bounds\n");
        return;
    }
    if !is_cpu_online(cpu) {
        kprint!("cpu {:u} is not online\n", cpu);
        return;
    }

    // body of the debug logic, to run on cpu 0
    extern "C" fn dap_debug_thread(arg: *mut c_void) -> c_int {
        let _pd = AutoPreemptDisabler::new();

        let cpu = arg.addr() as cpu_num_t;
        kprint!("victim cpu {:u}\n", cpu);

        let mut state = MaybeUninit::<Arm64DapProcessorState>::uninit();
        // SAFETY: Only `cpu_debug_command` starts this thread, and its caller keeps `cpu` powered
        // up until this read has finished. This runs as a kernel thread, so the calling CPU's
        // current `Thread` is initialized.
        if let Err(err) = unsafe { arm64_dap_read_processor_state(cpu, &mut state) } {
            kprint!("failed to read processor state, err {:d}\n", err.into_raw());
            return err.into_raw();
        }

        // SAFETY: `arm64_dap_read_processor_state` returned `Ok`, so it filled in all of `state`.
        let state = unsafe { state.assume_init_ref() };
        // SAFETY: `stdout()` points to `gStdout`, a valid `FILE` once its global constructor has
        // run, long before the console runs commands.
        unsafe { state.dump(stdout()) };

        ZX_OK
    }

    // create a thread to run on cpu 0 to run this
    // SAFETY: The name is a C string literal, and `dap_debug_thread` uses `arg` only as an integer,
    // so nothing has to outlive the call. Console commands run on a thread that may block, with no
    // `ChainLockTransaction`.
    let Ok(thread) = (unsafe {
        create_with_priority(
            c"dap debug".as_ptr(),
            dap_debug_thread,
            core::ptr::without_provenance_mut(cpu as usize),
            DEFAULT_PRIORITY,
        )
    }) else {
        kprint!("failed to create thread\n");
        return;
    };
    // SAFETY: `thread` was just created and has not been resumed yet. The console runs commands
    // holding its command mutex, which is not `thread`'s lock or a scheduler queue lock, and with
    // no `ChainLockTransaction`.
    unsafe { thread.set_cpu_affinity(cpu_num_to_mask(0)) };
    // SAFETY: This function just created `thread`, has not joined, detached or resumed it, and does
    // not use it after this. The console runs commands holding its command mutex, which is not
    // `thread`'s lock or the thread list lock, and with no `ChainLockTransaction`.
    if let Err(err) = unsafe { thread.detach_and_resume() } {
        kprint!("failed to resume thread, err {:d}\n", err.into_raw());
    }
}

#[cfg(console_enabled)]
fn dump() {
    let mdrar = MDRAR_EL1.read();
    kprint!("mdrar {:cs}{:x}\n", alt_hex_prefix(mdrar), mdrar);
    let dbgauthstatus = DBGAUTHSTATUS_EL1.read();
    kprint!("dbgauthstatus {:cs}{:x}\n", alt_hex_prefix(dbgauthstatus), dbgauthstatus);

    let apertures = dap_apertures();
    let daps = daps();
    if apertures.is_empty() || daps.is_empty() {
        kprint!("DAP not detected\n");
        return;
    }

    for da in apertures {
        let virt = da.virt.load(Ordering::Relaxed).addr() as u64;
        kprint!(
            "DAP aperture at {:cs}{:x}, length {:cs}{:x}\n",
            alt_hex_prefix(virt),
            virt,
            alt_hex_prefix(da.size as u64),
            da.size
        );
    }

    // Relaxed: these loads are only printed.
    for dap in daps {
        let dap_addr = dap.dap.load(Ordering::Relaxed).addr() as u64;
        let cti_addr = dap.cti.load(Ordering::Relaxed).addr() as u64;
        kprint!(
            "cpu {:u} DAP {:cs}{:x} CTI {:cs}{:x} initialized {:d}\n",
            dap.cpu_num.load(Ordering::Relaxed),
            alt_hex_prefix(dap_addr),
            dap_addr,
            alt_hex_prefix(cti_addr),
            cti_addr,
            dap.initialized.load(Ordering::Relaxed)
        );
    }
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs` whose `arg_str` fields point to
/// NUL-terminated strings, and `argc` is at least 1. For `cpu_debug <n>`, CPU `n` must stay
/// powered up until the `dap debug` thread the command starts has finished its read.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_dap(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprint!("usage:\n");
        kprint!("{:cs} dump\n", argv[0].arg_str);
        kprint!("{:cs} cpu_debug <n>\n", argv[0].arg_str);
        Status::INTERNAL.into_raw()
    };
    let not_enough_args = || -> c_int {
        kprint!("not enough arguments\n");
        usage()
    };

    if argc < 2 {
        return not_enough_args();
    }

    if argv[1].as_str() == "dump" {
        dump();
    } else if argv[1].as_str() == "cpu_debug" {
        if argc < 3 {
            return not_enough_args();
        }
        let Ok(cpu) = cpu_num_t::try_from(argv[2].arg_uint) else {
            kprint!("invalid cpu\n");
            return usage();
        };
        // SAFETY: The caller keeps CPU `n`, which is `cpu`, powered up until the thread's read has
        // finished.
        unsafe { cpu_debug_command(cpu) };
    } else {
        kprint!("unknown command\n");
        return usage();
    }

    ZX_OK
}

#[cfg(console_enabled)]
static_command!(CMD_DAP, c"dap".as_ptr(), c"arm debug port".as_ptr(), cmd_dap, CMD_AVAIL_NORMAL);

// External routines

/// Drops the victim CPU into debug state and reads its registers into `state`, all of it when
/// this returns `Ok`. A read that starts while another read of `victim` is in progress returns
/// `Status::UNAVAILABLE`.
///
/// NOTE: will leave the cpu in a stuck state. Also makes no attempt to validate
/// that the current cpu is the victim cpu. Suggest pinning the code to a single
/// cpu that is something other than the victim.
///
/// Always inlined: in the lockup path's caller, the read's locals cost less than a frame of their
/// own.
///
/// # Safety
///
/// - `victim` must stay powered up for the duration of the call, not offline or in a power-down
///   state, so that its debug registers can be accessed without a fault.
/// - Unless interrupts are disabled on the calling CPU, its current `Thread` must be initialized:
///   the read then disables preemption, which reaches it.
///
/// # Correctness
///
/// Breaking these gives wrong results, a panic or a hang, but not undefined behavior:
///
/// - `victim` must be less than `max_num_cpus()`, or this panics.
/// - `victim` must not be the calling CPU, and the caller must not be able to migrate to `victim`
///   for the duration of the call, or this halts the calling CPU.
/// - Once `victim` has halted, this leaves it halted, with its `x0` clobbered, and nothing
///   restarts it. Call this only where that is acceptable, such as a terminal kernel panic or an
///   operator's debugging command.
#[inline(always)]
pub unsafe fn arm64_dap_read_processor_state(
    victim: cpu_num_t,
    state: &mut MaybeUninit<Arm64DapProcessorState>,
) -> Result<(), Status> {
    // pin ourselves to the current cpu
    // With interrupts disabled the caller is already pinned. Skip the disabler then, so that its
    // drop cannot flush a pending preemption in the middle of a terminal panic.
    let mut pd = AutoPreemptDisabler::new_deferred();
    if !ints_disabled() {
        // The caller's `# Safety` gives this CPU a current `Thread` while interrupts are enabled.
        pd.disable();
    }

    let Some(daps) = enabled_daps() else {
        return Err(Status::BAD_STATE);
    };
    // find the CTI for this cpu
    let port = &daps[victim as usize];
    // Two reads of one victim would interleave their EDITR instructions and DCC reads. A reader
    // that panics leaves this set, which only refuses later reads while the panic halts the system.
    if port.reading.swap(true, Ordering::Acquire) {
        kprint!("DAP: cpu {:u} is already being read\n", victim);
        return Err(Status::UNAVAILABLE);
    }
    let _reading = zr::defer(|| port.reading.store(false, Ordering::Release));
    // Relaxed: `enabled_daps` observed `initialized` with Acquire, which publishes these.
    let ctir = port.cti.load(Ordering::Relaxed);
    let dapr = port.dap.load(Ordering::Relaxed);

    // SAFETY: `enabled_daps` saw `initialized` set, so both are non-null pointers to the
    // components `victim` found in an aperture that `arm_dap_init` mapped page-aligned and
    // read-write, and never unmaps, stored under the archid matching their type. `RomTable::walk`
    // passes only offsets that are multiples of `MINIMUM_COMPONENT_SIZE` and keep the component's
    // first `MINIMUM_COMPONENT_SIZE` bytes inside the aperture, so both are aligned and fully
    // mapped. CTI registers are in the debug power domain and have no error response.
    let cti = unsafe { RegBlock::<CtiRegs>::new(ctir) };
    // SAFETY: As above for the pointer. `DapDebugRegs` return an error response only while the
    // core is powered down, and the caller keeps `victim` powered up for the call.
    let dap_debug = unsafe { RegBlock::<DapDebugRegs>::new(dapr) };
    ltracef!("dbgauthstatus {:#x}\n", dap_debug.read(DapDebugRegs::DBGAUTHSTATUS));

    // EDPRSR is in the debug power domain, so it reads even while the core is powered down, unless
    // FEAT_DoPD puts it in the core one. PU set and DLK, OSLK and R clear mean that the
    // core-domain registers can be accessed.
    let edprsr = dap_debug.read(DapDebugRegs::EDPRSR);
    if (edprsr & (EDPRSR_DLK | EDPRSR_OSLK | EDPRSR_R | EDPRSR_PU)) != EDPRSR_PU {
        kprint!(
            "DAP: cpu {:u} is powered down, in reset or OS-locked: EDPRSR {:#x}\n",
            victim,
            edprsr
        );
        return Err(Status::BAD_STATE);
    }
    // SAFETY: As above for the pointer. `DapCoreRegs` return an error response while the core is
    // powered down, OS-locked or double-locked, and EDPRSR just reported none of those, nor a
    // reset. The caller keeps `victim` powered up for the call. Nothing in the kernel sets either
    // lock: it writes OSLAR_EL1 only with 0, and OSDLR_EL1 only with a value saved from it.
    let dap = unsafe { RegBlock::<DapCoreRegs>::new(dapr) };
    // CTIOUTEN1 routes channels to output trigger 1, the victim's restart request, and an event on
    // any routed channel would restart the victim once halted. CTI registers read while the CTI
    // is locked and disabled, so this refusal writes nothing either.
    let outen1 = cti.read(CtiRegs::CTIOUTEN1);
    if outen1 != 0 {
        kprint!(
            "DAP: cpu {:u}'s CTI routes a channel to restart: CTIOUTEN1 {:#x}\n",
            victim,
            outen1
        );
        return Err(Status::BAD_STATE);
    }

    // try to unlock the dap
    ltracef!("edlsr {:#x}\n", dap_debug.read(DapDebugRegs::EDLSR));
    // SAFETY: This is EDLAR's unlock key, which only opens the other debug registers to writes.
    unsafe { dap_debug.write(DapDebugRegs::EDLAR, CORESIGHT_UNLOCK_KEY) };
    ltracef!("edlsr {:#x}\n", dap_debug.read(DapDebugRegs::EDLSR));

    // unlock the cti
    ltracef!("ctilsr {:#x}\n", cti.read(CtiRegs::CTILSR));
    // SAFETY: This is CTILAR's unlock key, which only opens the other CTI registers to writes.
    unsafe { cti.write(CtiRegs::CTILAR, CORESIGHT_UNLOCK_KEY) };
    ltracef!("ctilsr {:#x}\n", cti.read(CtiRegs::CTILSR));

    // enable the cti
    ltracef!("cticontrol {:#x}\n", cti.read(CtiRegs::CTICONTROL));
    // SAFETY: CTICONTROL.GLBEN only enables this CTI, and changes no memory.
    unsafe { cti.write(CtiRegs::CTICONTROL, CTICONTROL_GLBEN) }; // make sure CTI is enabled

    // try to put the victim cpu in debug mode
    ltracef!("ctigate {:#x}\n", cti.read(CtiRegs::CTIGATE));
    // SAFETY: Zero in CTIGATE only stops channel events passing between this CTI and the others.
    unsafe { cti.write(CtiRegs::CTIGATE, 0) }; // mask off all internal channels
    ltracef!("ctigate {:#x}\n", cti.read(CtiRegs::CTIGATE));
    // generate input event to channel 0 debug request
    // SAFETY: CTIOUTEN0 routes channel 0 to this CTI's output trigger 0, the victim's debug
    // request. That request can halt the victim, and a halted CPU runs only what EDITR gives
    // it, which `run_instruction`'s `# Safety` limits.
    unsafe { cti.write(CtiRegs::CTIOUTEN0, CTI_CHANNEL_0) };
    // SAFETY: CTIAPPPULSE pulses channel 0, which asks the victim to halt, and a halted CPU runs
    // only what EDITR gives it. CTIGATE keeps the pulse inside this CTI, and of its output
    // triggers only trigger 1 restarts the victim, which the check above found no channel
    // reaches.
    unsafe { cti.write(CtiRegs::CTIAPPPULSE, CTI_CHANNEL_0) }; // generate debug event

    // read the status register of the processor
    if let Err(err) = dap_debug.wait_for(
        DapDebugRegs::EDPRSR,
        EDPRSR_DLK | EDPRSR_HALTED | EDPRSR_PU,
        EDPRSR_HALTED | EDPRSR_PU,
    ) {
        // Withdraw the debug request: a victim that has not halted by the acknowledgement below
        // does not halt later. Clear its route first, so that nothing raises it again after the
        // acknowledgement.
        // SAFETY: Zero in CTIOUTEN0 only stops channel events from raising the victim's debug
        // request.
        unsafe { cti.write(CtiRegs::CTIOUTEN0, 0) };
        // SAFETY: CTIINTACK only acknowledges the debug request, and does not restart the victim.
        unsafe { cti.write(CtiRegs::CTIINTACK, CTI_TRIGGER_DEBUG_REQUEST) };
        kprint!(
            "DAP: failed to drop cpu {:u} into debug mode, error {:d}\n",
            victim,
            err.into_raw()
        );
        return Err(err);
    }

    // cpu is stopped
    kprint!("DAP: cpu {:u} is in debug state\n", victim);

    // ack the CTI
    // SAFETY: CTIINTACK only acknowledges the debug request, and does not restart the victim.
    unsafe { cti.write(CtiRegs::CTIINTACK, CTI_TRIGGER_DEBUG_REQUEST) };

    // shove a nop down the hole to see if it works
    // SAFETY: `ARM64_NOP` does nothing. The wait above saw EDPRSR report the victim halted, and
    // the CTIINTACK write does not restart it.
    if let Err(err) = unsafe { run_instruction(&dap, ARM64_NOP) } {
        kprint!("DAP: failed to run first instruction on cpu, error {:d}\n", err.into_raw());
        return Err(err);
    }

    // load the full state of the cpu
    // SAFETY: The `wait_for` on EDPRSR above returned `Ok`, or this function would have returned
    // its error, so EDPRSR reported the victim halted. Nothing in the kernel restarts a halted
    // CPU: this file writes no restart request, and no `ARM64_*` instruction leaves Debug state.
    // An external debugger could, and that is outside this contract.
    if let Err(err) = unsafe { read_processor_state(&dap, state) } {
        kprint!("DAP: failed to read processor state, error {:d}\n", err.into_raw());
        return Err(err);
    }

    // TODO(https://fxbug.dev/587037925): restart the cpu

    Ok(())
}

/// Returns the per-CPU ports if every CPU has initialized its own, or `None`.
///
/// Out of line: inlined, it grows the frame that `arm64_dap_read_processor_state` is inlined into,
/// which the lockup path holds across `RegBlock::wait_for`.
#[inline(never)]
fn enabled_daps() -> Option<&'static [DebugPort]> {
    let daps = daps();
    if dap_apertures().is_empty() || daps.is_empty() {
        return None;
    }

    // see if we've detected and initialized DAP ports for all the cpus
    for dap in daps {
        // Acquire: pairs with the Release store in `arm_dap_init_percpu`, so a caller that gets
        // the ports sees every CPU's `dap` and `cti`.
        if !dap.initialized.load(Ordering::Acquire) {
            return None;
        }
    }

    Some(daps)
}

/// Returns true if DAP ports have been detected and initialized for all the CPUs. When it does,
/// every CPU below `max_num_cpus()` has run `arm_dap_init_percpu`, so it has started.
#[must_use]
pub fn arm64_dap_is_enabled() -> bool {
    enabled_daps().is_some()
}

impl Arm64DapProcessorState {
    /// bits [9:8] of EDSCR is the EL level of the cpu
    #[inline]
    #[must_use]
    pub fn get_el_level(&self) -> u8 {
        ((self.edscr >> 8) & 0x3) as u8
    }

    /// Prints the register state to `fp`, through `fprintf`: `core::fmt` nests too deep for the
    /// lockup path.
    ///
    /// # Safety
    ///
    /// `fp` must point to a valid `FILE` for the duration of the call.
    pub unsafe fn dump(&self, fp: *mut FILE) {
        let r = &self.r;
        // SAFETY: The caller guarantees `fp`, and each format's specifiers match its arguments: a
        // `u64` for `%lx`, a `u32` for `%x` and `%u`.
        unsafe {
            fprintf(
                fp,
                c"x0  %#18lx x1  %#18lx x2  %#18lx x3  %#18lx\n".as_ptr(),
                r[0],
                r[1],
                r[2],
                r[3],
            );
            fprintf(
                fp,
                c"x4  %#18lx x5  %#18lx x6  %#18lx x7  %#18lx\n".as_ptr(),
                r[4],
                r[5],
                r[6],
                r[7],
            );
            fprintf(
                fp,
                c"x8  %#18lx x9  %#18lx x10 %#18lx x11 %#18lx\n".as_ptr(),
                r[8],
                r[9],
                r[10],
                r[11],
            );
            fprintf(
                fp,
                c"x12 %#18lx x13 %#18lx x14 %#18lx x15 %#18lx\n".as_ptr(),
                r[12],
                r[13],
                r[14],
                r[15],
            );
            fprintf(
                fp,
                c"x16 %#18lx x17 %#18lx x18 %#18lx x19 %#18lx\n".as_ptr(),
                r[16],
                r[17],
                r[18],
                r[19],
            );
            fprintf(
                fp,
                c"x20 %#18lx x21 %#18lx x22 %#18lx x23 %#18lx\n".as_ptr(),
                r[20],
                r[21],
                r[22],
                r[23],
            );
            fprintf(
                fp,
                c"x24 %#18lx x25 %#18lx x26 %#18lx x27 %#18lx\n".as_ptr(),
                r[24],
                r[25],
                r[26],
                r[27],
            );
            fprintf(
                fp,
                c"x28 %#18lx x29 %#18lx lr  %#18lx sp  %#18lx\n".as_ptr(),
                r[28],
                r[29],
                r[30],
                self.sp,
            );
            fprintf(fp, c"\n".as_ptr());
            fprintf(fp, c"pc      %#18lx\n".as_ptr(), self.pc);
            fprintf(fp, c"cpsr    %#18lx\n".as_ptr(), self.cpsr);
            fprintf(
                fp,
                c"edscr   %#18x: EL %u\n".as_ptr(),
                self.edscr,
                u32::from(self.get_el_level()),
            );
            fprintf(fp, c"\n".as_ptr());
            if self.get_el_level() >= 1 {
                fprintf(
                    fp,
                    c"elr_el1 %#18lx far_el1 %#18lx esr_el1 %#18lx\n".as_ptr(),
                    self.elr_el1,
                    self.far_el1,
                    self.esr_el1,
                );
            }
            if self.get_el_level() >= 2 {
                fprintf(
                    fp,
                    c"elr_el2 %#18lx far_el2 %#18lx esr_el2 %#18lx\n".as_ptr(),
                    self.elr_el2,
                    self.far_el2,
                    self.esr_el2,
                );
            }
        }
    }
}
