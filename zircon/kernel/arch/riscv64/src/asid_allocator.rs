// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Address Space ID (ASID) allocator for RISC-V 64 address spaces.

use core::pin::Pin;
use debug::ltracef;
use id_allocator::IdAllocator;
use lazy_init::LazyInit;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

/// Number of ASID bits supported on RISC-V 64 Sv39/Sv48/Sv57.
const MMU_RISCV64_ASID_BITS: usize = 16;

/// Unused ASID value (used when ASID tagging is disabled).
pub const MMU_RISCV64_UNUSED_ASID: u16 = 0;

/// Hard-assigned ASID for the kernel address space.
pub const MMU_RISCV64_KERNEL_ASID: u16 = 1;

/// First ASID available for user processes.
const MMU_RISCV64_FIRST_USER_ASID: u16 = 2;

/// Maximum ASID value available for user processes (65535).
const MMU_RISCV64_MAX_USER_ASID: u16 = ((1u32 << MMU_RISCV64_ASID_BITS) - 1) as u16;

/// Total number of IDs in the 16-bit ASID space.
const MAX_ASIDS: usize = 65536;
const BITMAP_WORDS: usize = MAX_ASIDS / (usize::BITS as usize);

type GlobalAsidAllocator =
    IdAllocator<u16, MAX_ASIDS, { MMU_RISCV64_FIRST_USER_ASID as usize }, BITMAP_WORDS>;

/// Constructed once by [`asid_allocator_init`] during early MMU init; the
/// allocator's own `KMutex` serializes every use after that.
static GLOBAL_ASID_ALLOCATOR: LazyInit<GlobalAsidAllocator> = LazyInit::uninit();

/// Initializes the global ASID allocator.  Called once from the boot CPU's early
/// MMU init, before any address space can be created.
pub fn asid_allocator_init() {
    // SAFETY: runs once on the boot CPU before any other hart is started and
    // before any caller of `asid_alloc`/`asid_free` exists.
    unsafe {
        Pin::static_ref(&GLOBAL_ASID_ALLOCATOR)
            .init_pin(GlobalAsidAllocator::init())
            .expect("ASID allocator init");
    }
}

/// Allocate a new unique ASID in the range `[MMU_RISCV64_FIRST_USER_ASID, MMU_RISCV64_MAX_USER_ASID]`.
pub fn asid_alloc() -> Result<u16, Status> {
    // Exhaustion reports NO_MEMORY, the status the C++ allocator returned.
    let new_asid = GLOBAL_ASID_ALLOCATOR.try_alloc().map_err(|_| Status::NO_MEMORY)?;
    ltracef!("new asid {new_asid:#x}\n");
    Ok(new_asid)
}

/// Free a previously allocated ASID.
pub fn asid_free(asid: u16) -> Result<(), Status> {
    ltracef!("free asid {asid:#x}\n");
    GLOBAL_ASID_ALLOCATOR.free(asid)
}
