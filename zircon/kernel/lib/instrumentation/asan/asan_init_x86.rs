// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86-64 KASAN shadow page table setup and initialization.

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr;

use kprint::kprintln;

use crate::arch_rs::ops::zero_page;
use crate::arch_rs::x86::mmu::{
    ADDR_OFFSET, NO_OF_PT_ENTRIES, PD_SHIFT, PDP_SHIFT, PT_SHIFT, X86_FLAGS_MASK,
    X86_KERNEL_KASAN_PD_FLAGS, X86_KERNEL_KASAN_RW_PT_FLAGS, X86_MMU_PG_NX, X86_MMU_PG_RW,
    x86_upper_512gib_page_table,
};
use crate::counters::define_kcounter;
use crate::kernel::types::{PAddr, VAddr};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE,
};
use crate::vm::page_state::VmPageState;
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm;
use crate::vm::pmm_arena::PmmArenaInfo;
use crate::vm::vm::{vm_get_kernel_heap_base, vm_get_kernel_heap_size, vm_using_virtual_heap};
use crate::vm::vm_aspace::VmAspace;

use super::KASAN_SHADOW_OFFSET;
use super::asan_internal::{ASAN_SHADOW_SIZE, EarlyBootCell, addr2shadow, set_asan_initialized};

const PT_MASK: usize = (1 << ADDR_OFFSET) - 1;

define_kcounter!(ASAN_ALLOCATED_SHADOW_PAGES, "asan.allocated_shadow_pages", Sum);
define_kcounter!(ASAN_ALLOCATED_SHADOW_PAGE_TABLES, "asan.allocated_shadow_page_tables", Sum);

unsafe extern "C" {
    static kasan_shadow_pt: [u64; NO_OF_PT_ENTRIES];
    static kasan_shadow_pd: [u64; NO_OF_PT_ENTRIES];

    fn __unsanitized_memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
}

static ASAN_CAN_MAP_SHADOW_FOR: EarlyBootCell<bool> = EarlyBootCell::new(true);

#[sanitize(address = "off")]
#[inline]
fn can_map_shadow_for() -> bool {
    // SAFETY: `ASAN_CAN_MAP_SHADOW_FOR` is written once during single-threaded boot
    // (`arch_asan_late_init`) before secondary CPUs can call `asan_map_shadow_for`.
    unsafe { ASAN_CAN_MAP_SHADOW_FOR.get() }
}

/// Disallows further calls to `asan_map_shadow_for` after early boot.
///
/// # Safety
///
/// Must only be called once during single-threaded boot (`LK_INIT_LEVEL_VM`) before secondary
/// CPUs start.
#[sanitize(address = "off")]
#[inline]
unsafe fn disallow_map_shadow_for() {
    // SAFETY: Caller guarantees single-threaded boot (`LK_INIT_LEVEL_VM`).
    unsafe { ASAN_CAN_MAP_SHADOW_FOR.set(false) }
}

#[sanitize(address = "off")]
#[inline]
const fn vaddr_to_pdp_index(addr: usize) -> usize {
    (addr >> PDP_SHIFT) & PT_MASK
}

#[sanitize(address = "off")]
#[inline]
const fn vaddr_to_pd_index(addr: usize) -> usize {
    (addr >> PD_SHIFT) & PT_MASK
}

#[sanitize(address = "off")]
#[inline]
const fn vaddr_to_pt_index(addr: usize) -> usize {
    (addr >> PT_SHIFT) & PT_MASK
}

/// # Safety
///
/// `table.wrapping_add(i)` must be a valid page table entry in the physmap, and `initial_value`
/// must point to a `page::SIZE`-byte template page table.
#[sanitize(address = "off")]
unsafe fn get_or_allocate_page_table(
    table: *mut u64,
    i: usize,
    initial_value: *const u64,
) -> PAddr {
    // SAFETY: Caller guarantees `table.wrapping_add(i)` is a valid page table entry in the physmap.
    let pdp_entry = unsafe { *table.wrapping_add(i) };

    // The table is either empty or has X86_KERNEL_KASAN_PD_INITIAL_FLAGS
    // In either case, it doesn't have Write permission.
    if (pdp_entry & X86_MMU_PG_RW) != 0 {
        return PAddr((pdp_entry & !X86_FLAGS_MASK) as usize);
    }

    let res = pmm::alloc_page(0);
    debug_assert!(res.is_ok());
    let (pd_page, pd_page_paddr) = res.unwrap();
    // SAFETY: `pd_page` is a newly allocated PMM page exclusively owned by the caller.
    unsafe {
        pd_page.as_ref().set_state(VmPageState::MMU);
    }
    ASAN_ALLOCATED_SHADOW_PAGE_TABLES.add(1);
    let dst = ptr::with_exposed_provenance_mut::<c_void>(paddr_to_physmap(pd_page_paddr).0);
    // SAFETY: `dst` is a newly allocated page in the physmap and `initial_value` points to a
    // `page::SIZE`-byte template page table.
    unsafe {
        __unsanitized_memcpy(dst, initial_value.cast::<c_void>(), page::SIZE);
    }
    pd_page_paddr
}

/// `asan_remap_shadow_internal` updates the KASAN shadow map to allow poisoning in the region
/// `[start, start + size)`.
///
/// # Safety
///
/// `pdp` must point to the 512-entry upper 512 GiB PDP table in the physmap, and
/// `[start, start + size)` must be a valid kernel virtual address range (`size > 0`) during
/// single-threaded early boot.
#[sanitize(address = "off")]
unsafe fn asan_remap_shadow_internal(pdp: *mut u64, start: usize, size: usize) {
    let start_shadow = addr2shadow(start).addr();
    let end_shadow = addr2shadow(start.wrapping_add(size - 1)).addr();
    kprintln!(
        "KASAN: shadow for [{:#x}, {:#x}) at [{:#x}, {:#x})",
        start,
        start.wrapping_add(size),
        start_shadow,
        end_shadow
    );
    let pdp_map_start = vaddr_to_pdp_index(start_shadow);
    let pdp_map_end = vaddr_to_pdp_index(end_shadow);
    let pd_map_start = vaddr_to_pd_index(start_shadow);
    let pd_map_end = vaddr_to_pd_index(end_shadow);
    let pt_map_start = vaddr_to_pt_index(start_shadow);
    let pt_map_end = vaddr_to_pt_index(end_shadow);

    let shadow_pd_ptr = (&raw const kasan_shadow_pd).cast::<u64>();
    let shadow_pt_ptr = (&raw const kasan_shadow_pt).cast::<u64>();

    // TODO(https://fxbug.dev/42127444): When pmm_alloc_page allows getting high memory, use high
    // pages where possible for page tables and asan shadow pages.
    let mut i = pdp_map_start;
    while i <= pdp_map_end {
        // SAFETY: `pdp` points to the 512-entry upper 512 GiB PDP table and `shadow_pd_ptr`
        // points to `kasan_shadow_pd`.
        let new_pdp_paddr = unsafe { get_or_allocate_page_table(pdp, i, shadow_pd_ptr) };
        let pd = ptr::with_exposed_provenance_mut::<u64>(paddr_to_physmap(new_pdp_paddr).0);
        let new_pdp_entry = (new_pdp_paddr.0 as u64) | X86_KERNEL_KASAN_PD_FLAGS;

        let mut j = 0;
        while j < NO_OF_PT_ENTRIES {
            // The first and last page directories might have entries that do not
            // belong to this request. Skip them.
            if (i == pdp_map_start && j < pd_map_start) || (i == pdp_map_end && j > pd_map_end) {
                j += 1;
                continue;
            }

            // SAFETY: `pd` points to a 512-entry page directory in the physmap and `shadow_pt_ptr`
            // points to `kasan_shadow_pt`.
            let new_pd_paddr = unsafe { get_or_allocate_page_table(pd, j, shadow_pt_ptr) };
            let pt = ptr::with_exposed_provenance_mut::<u64>(paddr_to_physmap(new_pd_paddr).0);
            let new_pd_entry = (new_pd_paddr.0 as u64) | X86_KERNEL_KASAN_PD_FLAGS;

            // Allocate and fill in leaf page tables for shadow map.
            let mut k = 0;
            while k < NO_OF_PT_ENTRIES {
                // The first and last page tables might have entries that do not belong
                // to this request. Skip them.
                if (i == pdp_map_start && j == pd_map_start && k < pt_map_start)
                    || (i == pdp_map_end && j == pd_map_end && k > pt_map_end)
                {
                    k += 1;
                    continue;
                }

                // SAFETY: `pt.wrapping_add(k)` is a valid leaf page table entry in the physmap.
                let pt_entry = unsafe { *pt.wrapping_add(k) };
                if (pt_entry & X86_MMU_PG_RW) != 0 {
                    // already mapped.
                    k += 1;
                    continue;
                }

                let res = pmm::alloc_page(0);
                debug_assert!(res.is_ok());
                let (rw_page, rw_page_paddr) = res.unwrap();
                // SAFETY: `rw_page` is a newly allocated PMM page exclusively owned by the caller.
                unsafe {
                    rw_page.as_ref().set_state(VmPageState::WIRED);
                }
                ASAN_ALLOCATED_SHADOW_PAGES.add(1);
                // SAFETY: `paddr_to_physmap(rw_page_paddr)` is a newly allocated page in the
                // physmap.
                unsafe {
                    zero_page(paddr_to_physmap(rw_page_paddr));
                    *pt.wrapping_add(k) =
                        (rw_page_paddr.0 as u64) | X86_KERNEL_KASAN_RW_PT_FLAGS | X86_MMU_PG_NX;
                }
                k += 1;
            }
            // SAFETY: `pd.wrapping_add(j)` is a valid page directory entry in the physmap.
            unsafe {
                *pd.wrapping_add(j) = new_pd_entry;
            }
            j += 1;
        }
        // SAFETY: `pdp.wrapping_add(i)` is a valid PDP entry in the physmap.
        unsafe {
            *pdp.wrapping_add(i) = new_pdp_entry;
        }
        i += 1;
    }
    // Walk the entire area we just added and invalidate TLB entries. Shootdown not required since
    // we are single-threaded at this point in boot.
    let mut addr = page::round_down(start_shadow);
    let last_page = page::round_down(end_shadow);
    loop {
        // SAFETY: `invlpg` invalidates the TLB entry for `addr` and has no memory safety side
        // effects.
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
        }
        if addr == last_page {
            break;
        }
        addr = addr.wrapping_add(page::SIZE);
    }
}

/// Adds the virtual region defined by `[start, start + size)` to the regions
/// instrumented by asan. After calling this function, users can call
/// `asan_poison_shadow` on the bytes in the newly added region.
/// On x86-64 This function can only be called before SMP is set up.
/// TODO(https://fxbug.dev/42104852): Allow calling after SMP is set up.
///
/// # Safety
///
/// Prior to `arch_asan_late_init` (`LK_INIT_LEVEL_VM`), the caller must ensure that `size > 0`,
/// `[start, start + size)` is a valid kernel virtual address range within the KASAN shadow-mapped
/// kernel address space, and execution is single-threaded prior to SMP bringup. After
/// `arch_asan_late_init`, `can_map_shadow_for()` is `false` and this function returns immediately
/// as a no-op.
#[sanitize(address = "off")]
pub unsafe fn asan_map_shadow_for(start: usize, size: usize) {
    // TODO(https://fxbug.dev/42104852): Allow calling map_shadow_for after early boot.
    if !can_map_shadow_for() {
        return;
    }
    // SAFETY: Called during single-threaded early boot with a valid kernel virtual address range.
    unsafe {
        asan_remap_shadow_internal(x86_upper_512gib_page_table(), start, size);
    }
}

/// # Safety
///
/// Must only be called once during single-threaded early boot after PMM initialization.
#[sanitize(address = "off")]
unsafe fn arch_asan_reallocate_shadow() {
    let num_arenas = pmm::num_arenas();
    let mut i = 0;
    while i < num_arenas {
        let mut arena = MaybeUninit::<PmmArenaInfo>::uninit();
        pmm::get_arena_info(i, core::slice::from_mut(&mut arena)).expect("pmm::get_arena_info");
        // SAFETY: `pmm::get_arena_info` succeeded and initialized `arena`.
        let arena = unsafe { arena.assume_init_ref() };
        let address = paddr_to_physmap(arena.base).0;
        // SAFETY: `[address, address + arena.size)` is a valid physmap arena range during
        // single-threaded early boot.
        unsafe {
            asan_map_shadow_for(address, arena.size);
        }
        i += 1;
    }
    if vm_using_virtual_heap() {
        // SAFETY: `[vm_get_kernel_heap_base(), +vm_get_kernel_heap_size())` is a valid kernel
        // heap virtual address range during single-threaded early boot.
        unsafe {
            asan_map_shadow_for(vm_get_kernel_heap_base(), vm_get_kernel_heap_size());
        }
    }
}

/// KASAN initialization after the PMM is available.
///
/// # Safety
///
/// Must only be called once during single-threaded early boot (`LK_INIT_LEVEL_VM_PREHEAP`).
#[sanitize(address = "off")]
pub(super) unsafe fn arch_asan_early_init() {
    // SAFETY: Caller guarantees single-threaded early boot after PMM initialization.
    unsafe {
        arch_asan_reallocate_shadow();
        set_asan_initialized();
    }
}

/// KASAN initialization after the VM/kernel_aspace are available and we can update kernel mappings.
///
/// # Safety
///
/// Must only be called once during single-threaded boot (`LK_INIT_LEVEL_VM`).
#[sanitize(address = "off")]
pub(super) unsafe fn arch_asan_late_init() {
    // SAFETY: `[KASAN_SHADOW_OFFSET, +ASAN_SHADOW_SIZE)` is already mapped in the kernel page
    // tables and we are reserving its virtual address range in the root kernel VMAR during boot.
    let status = unsafe {
        VmAspace::kernel_aspace().root_vmar().unwrap().reserve_space(
            c"kasan-shadow",
            VAddr(KASAN_SHADOW_OFFSET),
            ASAN_SHADOW_SIZE,
            ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
        )
    };
    assert!(status.is_ok());
    // SAFETY: Caller guarantees single-threaded boot (`LK_INIT_LEVEL_VM`).
    unsafe {
        disallow_map_shadow_for();
    }
}
