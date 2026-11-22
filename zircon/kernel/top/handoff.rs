// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr::{NonNull, null_mut};

use fbl::{RefPtr, Vector};
use handoff::{BootConstants, PhysBootTimes_Index, PhysElfImage, PhysHandoff, PhysVmo};
use init::LkInitLevel;
use libarch::EarlyTicks;
use page::{SIZE as PAGE_SIZE, is_page_rounded};
use zx_types::ZX_RIGHT_WRITE;

use crate::counters::{Counter, define_kcounter};
use crate::kernel::types::PAddr;
use crate::object::{HandleOwner, InitialMutability, VmObjectDispatcher};
use crate::platform_rs::timer::platform_convert_early_ticks;
use crate::userabi::{HandoffEnd, HandoffEndElf};
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm::{ALLOC_FLAG_ANY, end_handoff as pmm_end_handoff};
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;

/// Transparent `UnsafeCell` wrapper for boot-sequenced globals written only on the primary CPU
/// without concurrent readers.
#[repr(transparent)]
pub struct BootGlobal<T>(UnsafeCell<T>);

// SAFETY: Every `BootGlobal` instance is mutated exclusively on the primary CPU during a
// single-threaded boot phase (or before any reader CPU starts, or after all readers have
// finished), with no concurrent read/write access; `T: Send` permits moving/copying `T` across
// CPUs.
unsafe impl<T: Send> Sync for BootGlobal<T> {}

impl<T> BootGlobal<T> {
    pub(super) const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    /// Writes `val` into the boot global.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent reads or writes to this global can occur.
    #[inline(always)]
    pub(super) unsafe fn set(&self, val: T) {
        // SAFETY: Caller guarantees exclusive access with no concurrent readers or writers.
        unsafe { *self.0.get() = val };
    }
}

impl<T: Copy> BootGlobal<T> {
    /// Reads the current value of the boot global.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent writes to this global can occur.
    #[inline(always)]
    pub(super) unsafe fn get(&self) -> T {
        // SAFETY: Caller guarantees no concurrent writes are in progress.
        unsafe { *self.0.get() }
    }
}

/// Flag polled during `kernel.debug.boot-spin` until modified externally by a debugger.
#[cfg(any(lk_debuglevel = "2", lk_debuglevel_gt2))]
#[repr(transparent)]
pub struct DebugBootSpinFlag(UnsafeCell<u8>);

#[cfg(any(lk_debuglevel = "2", lk_debuglevel_gt2))]
// SAFETY: `DebugBootSpinFlag` is only read via `read_volatile` on the boot CPU in `lk_main` while
// spinning for an external debugger write.
unsafe impl Sync for DebugBootSpinFlag {}

#[cfg(any(lk_debuglevel = "2", lk_debuglevel_gt2))]
impl DebugBootSpinFlag {
    const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    /// Performs a volatile read of the flag, treating any non-zero byte written by a debugger as
    /// ready.
    #[inline(always)]
    pub(super) fn is_ready(&self) -> bool {
        // SAFETY: `self.0.get()` points to a valid static `u8`, and `u8` accepts any initialized
        // bit pattern written by a hardware or emulator debugger.
        unsafe { core::ptr::read_volatile(self.0.get()) != 0 }
    }
}

/// Raw `PhysHandoff` pointer stored in [`PHYS_HANDOFF`] (`gPhysHandoff`).
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PhysHandoffPtr(*mut PhysHandoff);

// SAFETY: When non-null during the hand-off phase, the `PhysHandoff` pointee resides in shared
// hand-off memory (with interior-mutable `entropy_pool` wrapped in `UnsafeCell` and consumed once
// on the primary CPU), so copying the pointer value across CPUs is sound.
unsafe impl Send for PhysHandoffPtr {}

// SAFETY: Exported symbol matches `extern PhysHandoff* gPhysHandoff` in `<phys/handoff/ptr.h>`
// (`#[repr(transparent)]` over `*mut PhysHandoff`) and has no other definition.
#[unsafe(export_name = "gPhysHandoff")]
pub static PHYS_HANDOFF: BootGlobal<PhysHandoffPtr> = BootGlobal::new(PhysHandoffPtr(null_mut()));

// SAFETY: Exported symbol matches `extern vaddr_t gPhysmapBase` in `<vm/physmap.h>`
// (`#[repr(transparent)]` over `usize`) and has no other definition.
#[unsafe(export_name = "gPhysmapBase")]
pub static PHYSMAP_BASE: BootGlobal<usize> = BootGlobal::new(0);

// SAFETY: Exported symbol matches `extern size_t gPhysmapSize` in `<vm/physmap.h>`
// (`#[repr(transparent)]` over `usize`) and has no other definition.
#[unsafe(export_name = "gPhysmapSize")]
pub static PHYSMAP_SIZE: BootGlobal<usize> = BootGlobal::new(0);

#[cfg(any(lk_debuglevel = "2", lk_debuglevel_gt2))]
// SAFETY: Exported symbol matches `extern constinit volatile bool gDebugBootSpinReady` in
// `<phys/handoff/zircon-abi-spec.h>` (1-byte size and alignment) and has no other definition.
#[unsafe(export_name = "gDebugBootSpinReady")]
pub static DEBUG_BOOT_SPIN_READY: DebugBootSpinFlag = DebugBootSpinFlag::new();

/// Returns the base virtual address of the kernel physmap.
#[inline(always)]
pub fn physmap_base() -> usize {
    // SAFETY: `gPhysmapBase` is initialized once in `post_handoff_bootstrap` at kernel entry
    // before any caller runs and is read-only thereafter.
    unsafe { PHYSMAP_BASE.get() }
}

/// Returns the size in bytes of the kernel physmap.
#[inline(always)]
pub fn physmap_size() -> usize {
    // SAFETY: `gPhysmapSize` is initialized once in `post_handoff_bootstrap` at kernel entry
    // before any caller runs and is read-only thereafter.
    unsafe { PHYSMAP_SIZE.get() }
}

unsafe extern "C" {
    static __executable_start: [u8; 0];
    static __data_end: [u8; 0];
    fn CodePatchingNopTest();
}

// Defines `kBootConstants` in `.rodata.kBootConstants` so the compiler cannot see its initializer
// and therefore cannot fold away reads of its contents. The link-time image is filled with
// distinctive `0xdd` bytes; before the kernel runs, physboot writes the real `BootConstants`
// contents here.
//
// TODO(https://fxbug.dev/379891035): arch/x86/start.S uses the
// kKernelPhysicalLoadAddress symbol.  It can be removed when that is gone.
const BOOT_CONSTANTS_FILL: u8 = 0xdd;
global_asm!(
    r#"
    .pushsection .rodata.kBootConstants, "a", %progbits
    .balign {alignment}
    .globl kBootConstants
    .hidden kBootConstants
    .type kBootConstants, %object
    kBootConstants:
      .space {size}, {fill}
    .size kBootConstants, {size}
    .globl kKernelPhysicalLoadAddress
    .hidden kKernelPhysicalLoadAddress
    .type kKernelPhysicalLoadAddress, %object
    kKernelPhysicalLoadAddress = kBootConstants + {offsetof_loadaddr}
    .popsection
    "#,
    alignment = const align_of::<BootConstants>(),
    size = const size_of::<BootConstants>(),
    fill = const BOOT_CONSTANTS_FILL,
    offsetof_loadaddr = const offset_of!(BootConstants, kernel_physical_load_address),
);

// When using physboot, other samples are available in the handoff data too.
//
// **NOTE** Each sample here is represented in the userland test code in
// //src/tests/benchmarks/kernel_boot_stats.cc that knows the order of the
// steps and gives names to the intervals between the steps (as well as
// tracking the first-to-last total elapsed time across the first to last
// boot.timeline.* samples, not all recorded right here).  Any time a new time
// sample is added to PhysBootTimes, a kcounter should be added here and
// kernel_boot_stats.cc should be updated to give the new intervals appropriate
// names for the performance tracking infrastructure (see the pages at
// https://chromeperf.appspot.com/report and look for "fuchsia.kernel.boot").
define_kcounter!(TIMELINE_HW_STARTUP, "boot.timeline.hw", Sum);
define_kcounter!(TIMELINE_ZBI_ENTRY, "boot.timeline.zbi", Sum);
define_kcounter!(TIMELINE_PHYSBOOT_SETUP, "boot.timeline.physboot-setup", Sum);
define_kcounter!(TIMELINE_DECOMPRESS_START, "boot.timeline.decompress-start", Sum);
define_kcounter!(TIMELINE_DECOMPRESS_END, "boot.timeline.decompress-end", Sum);
define_kcounter!(TIMELINE_ZBI_DONE, "boot.timeline.zbi-done", Sum);

fn set(counter: &Counter, sample: EarlyTicks) {
    counter.set(platform_convert_early_ticks(sample) as u64);
}

fn set_index(handoff: &PhysHandoff, counter: &Counter, i: PhysBootTimes_Index) {
    counter.set(platform_convert_early_ticks(handoff.times.get(i)) as u64);
}

// Convert early boot timeline points into zx_ticks_t values in kcounters.
fn timeline_counters(_level: LkInitLevel) {
    // SAFETY: `timeline_counters` runs once at `LK_INIT_LEVEL_PLATFORM`, after
    // `post_handoff_bootstrap` and before `end_handoff`.
    let handoff = unsafe { PhysHandoff::get() };

    // This isn't really a loop in any meaningful sense, but structuring it
    // this way gets the compiler to warn about any forgotten enum entry.
    for when in [
        PhysBootTimes_Index::ZbiEntry,
        PhysBootTimes_Index::PhysSetup,
        PhysBootTimes_Index::DecompressStart,
        PhysBootTimes_Index::DecompressEnd,
        PhysBootTimes_Index::ZbiDone,
        PhysBootTimes_Index::Count,
    ] {
        match when {
            PhysBootTimes_Index::ZbiEntry => set_index(handoff, &TIMELINE_ZBI_ENTRY, when),
            PhysBootTimes_Index::PhysSetup => set_index(handoff, &TIMELINE_PHYSBOOT_SETUP, when),
            PhysBootTimes_Index::DecompressStart => {
                set_index(handoff, &TIMELINE_DECOMPRESS_START, when)
            }
            PhysBootTimes_Index::DecompressEnd => {
                set_index(handoff, &TIMELINE_DECOMPRESS_END, when)
            }
            PhysBootTimes_Index::ZbiDone => set_index(handoff, &TIMELINE_ZBI_DONE, when),
            PhysBootTimes_Index::Count => {
                // There is no PhysBootTimes entry corresponding to Count.
            }
        }
    }
    set(&TIMELINE_HW_STARTUP, EarlyTicks::zero());
}

// This can happen really any time after the platform clock is configured.
init::lk_init_hook!(TimelineCounters, timeline_counters, init::LK_INIT_LEVEL_PLATFORM);

/// Wraps the wired physical pages described by `phys_vmo` in a new [`VmObject`].
///
/// # Safety
///
/// The kernel physmap must be initialized (`post_handoff_bootstrap`), and `phys_vmo` must describe
/// page-aligned, wired physical pages owned exclusively by the kernel and not already wrapped in
/// any VMO.
unsafe fn create_phys_vmo(phys_vmo: &PhysVmo) -> RefPtr<VmObject> {
    let name = phys_vmo.name();
    debug_assert!(!name.is_empty());

    let size_bytes = phys_vmo.size_bytes::<PAGE_SIZE>();
    debug_assert!(is_page_rounded(phys_vmo.addr));
    debug_assert!(is_page_rounded(size_bytes));

    let vaddr = paddr_to_physmap(PAddr(phys_vmo.addr));
    let data = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(vaddr.0))
        .expect("physmap address of a PhysVmo is non-null");
    // SAFETY: Caller guarantees the physmap is initialized and `phys_vmo` describes page-aligned,
    // wired pages exclusively owned by the kernel and not wrapped in any other VMO.
    let status = unsafe { VmObjectPaged::create_from_wired_pages_raw(data, size_bytes, true) };
    assert!(status.is_ok());
    let vmo = status.unwrap();

    let status = vmo.set_name(name.as_bytes());
    debug_assert!(status.is_ok());

    phys_vmo.log(c"VM");

    VmObjectPaged::into_vm_object(vmo)
}

fn create_handle(vmo: RefPtr<VmObject>, stream_size: usize, writable: bool) -> HandleOwner {
    let status = VmObjectDispatcher::create(&vmo, stream_size as u64, InitialMutability::Mutable);
    assert!(status.is_ok());
    let (handle, mut rights) = status.unwrap();

    if writable {
        rights |= ZX_RIGHT_WRITE;
    } else {
        rights &= !ZX_RIGHT_WRITE;
    }
    HandleOwner::make(handle, rights).expect("HandleOwner::make failed")
}

/// # Safety
///
/// The kernel physmap must be initialized (`post_handoff_bootstrap`), and `phys_vmo` must describe
/// page-aligned, wired physical pages owned exclusively by the kernel and not already wrapped in
/// any VMO.
unsafe fn create_phys_vmo_handle(phys_vmo: &PhysVmo, writable: bool) -> HandleOwner {
    // SAFETY: Forwarded from caller.
    create_handle(unsafe { create_phys_vmo(phys_vmo) }, phys_vmo.stream_size, writable)
}

fn create_stub_vmo_handle() -> HandleOwner {
    let status = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, 0);
    assert!(status.is_ok());
    create_handle(VmObjectPaged::into_vm_object(status.unwrap()), 0, false)
}

/// # Safety
///
/// Temporary hand-off memory backing `image.vmar` must still be mapped and live, the kernel
/// physmap must be initialized (`post_handoff_bootstrap`), and `image.vmo` must describe
/// page-aligned, wired physical pages owned exclusively by the kernel and not already wrapped in
/// any VMO.
unsafe fn create_phys_elf(image: &PhysElfImage) -> HandoffEndElf {
    debug_assert!(image.vmar.base == 0);
    let mut elf = HandoffEndElf {
        // SAFETY: Caller guarantees `image.vmo` satisfies `create_phys_vmo`'s preconditions.
        vmo: unsafe { create_phys_vmo(&image.vmo) },
        stream_size: image.vmo.stream_size,
        vmar_size: image.vmar.size,
        mappings: Vector::new(),
        info: image.info,
    };
    // SAFETY: Caller guarantees temporary hand-off memory remains live.
    let mappings = unsafe { image.vmar.mappings() };
    elf.mappings.reserve(mappings.len()).unwrap_or_else(|_| {
        panic!("no kernel heap space for ELF {} mappings in {}", mappings.len(), image.vmo.name(),)
    });
    for &mapping in mappings {
        elf.mappings.push_back(mapping).expect("reserved capacity for ELF mappings");
    }
    elf
}

/// This function is called first thing on kernel entry, so it should be
/// careful on what it assumes is present.
///
/// # Safety
///
/// `handoff` must be a valid non-null pointer to the `PhysHandoff` passed by physboot, and this
/// function must be called once on the boot CPU before any other threads or CPUs are active.
pub(super) unsafe fn post_handoff_bootstrap(handoff: *mut PhysHandoff) {
    // Crucial set-up happens in arch_rs::post_handoff_bootstrap() and it should
    // happen early. We take care to only sequence the simple setting of several,
    // fundamental global variables before then, along with entrypoint time
    // sampling.

    debug_assert!(!handoff.is_null());
    // SAFETY: Single-threaded early boot on CPU 0 before any readers of `gPhysHandoff` exist.
    unsafe { PHYS_HANDOFF.set(PhysHandoffPtr(handoff)) };

    debug_assert!(
        KernelPhysicalAddressOf((&raw const __executable_start).addr())
            == kernel_physical_load_address()
    );

    // SAFETY: `handoff` points to a valid `PhysHandoff` in temporary hand-off memory, and we
    // project individual fields via raw pointer during single-threaded early boot.
    unsafe {
        PHYSMAP_BASE.set((*handoff).physmap_base);
        PHYSMAP_SIZE.set((*handoff).physmap_size);
        crate::arch_rs::post_handoff_bootstrap(&raw const (*handoff).arch_handoff);
    }

    // This serves as a verification that code-patching was performed before
    // the kernel was booted; if unpatched, we would trap here and halt.
    // SAFETY: `CodePatchingNopTest` executes a patched `nop` sequence with no side effects.
    unsafe {
        CodePatchingNopTest();
    }
}

/// Returns the physical load address of the kernel image populated in `kBootConstants` by
/// physboot.
fn kernel_physical_load_address() -> usize {
    BootConstants::get().kernel_physical_load_address
}

/// Translates a virtual address inside the contiguous kernel file image
/// (`[__executable_start, __data_end)`) into its physical address.
// SAFETY: Exported symbol matches `extern "C" paddr_t KernelPhysicalAddressOf(uintptr_t va)` in
// `<vm/handoff-end.h>` and has no other definition.
#[unsafe(no_mangle)]
pub extern "C" fn KernelPhysicalAddressOf(va: usize) -> usize {
    // Only allow addresses within the bounds of the original file image, which
    // is physically contiguous.  The bss pages are not necessarily contiguous.
    let start = (&raw const __executable_start).addr();
    let end = (&raw const __data_end).addr();
    debug_assert!(va >= start, "{va:#x} < {start:#x}");
    debug_assert!(va < end, "{va:#x} >= {end:#x}");
    kernel_physical_load_address() + (va - start)
}

/// Formally ends the hand-off phase, unsetting `gPhysHandoff` and returning the
/// remaining hand-off data left to be consumed (in a userboot-friendly way),
/// and freeing temporary hand-off memory (see `PhysHandoff::temporary_memory`).
///
/// After the end of hand-off, all pointers previously referenced by
/// `gPhysHandoff` should be regarded as freed and unusable.
///
/// # Safety
///
/// Must be called at most once on the primary CPU after all consumers of `gPhysHandoff` and
/// temporary hand-off memory have completed, with no remaining references into temporary hand-off
/// memory.
pub unsafe fn end_handoff() -> HandoffEnd {
    // SAFETY: Caller guarantees `gPhysHandoff` is still valid prior to hand-off teardown, temporary
    // hand-off memory remains live until `vm_end_handoff` below, and each handed-off `PhysVmo` is
    // wrapped into a VMO only once here.
    let end = unsafe {
        let handoff = PhysHandoff::get();
        // If the number of extra VMOs from physboot is less than the number of VMOs
        // the userboot protocol expects, fill the rest with empty VMOs.
        let phys_vmos = handoff.extra_vmos();
        assert!(phys_vmos.len() <= PhysVmo::MAX_EXTRA_HANDOFF_PHYS_VMOS);
        HandoffEnd {
            // Userboot expects the ZBI as writable.
            zbi: create_phys_vmo_handle(&handoff.zbi, /*writable=*/ true),
            vdso: create_phys_elf(&handoff.vdso),
            userboot: create_phys_elf(&handoff.userboot),
            extra_phys_vmos: core::array::from_fn(|i| match phys_vmos.get(i) {
                Some(phys_vmo) => create_phys_vmo_handle(phys_vmo, /*writable=*/ false),
                None => create_stub_vmo_handle(),
            }),
        }
    };

    // Point of temporary hand-off memory expiration: clear `gPhysHandoff` first so it never
    // dangles into unmapped or freed pages, then unmap the temporary hand-off VMAR and free the
    // backing pages in the PMM.
    // SAFETY: Caller guarantees all hand-off consumers have finished and all references into
    // temporary hand-off memory were dropped at the end of the block above.
    unsafe {
        PHYS_HANDOFF.set(PhysHandoffPtr(null_mut()));
        crate::vm::vm::vm_end_handoff();
        pmm_end_handoff();
    }

    end
}
