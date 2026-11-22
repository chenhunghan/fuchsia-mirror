// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel address space allocator for `percpu_writer` buffers.

mod allocator_ffi;
pub mod buffer;

pub use buffer::{
    Buffer, DroppedRecordDurationEvent, DroppedRecordStats, Reservation, serialize_drop_stats,
};

use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::pmm::{ALLOC_FLAG_ANY, ALLOC_FLAG_CAN_WAIT};
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::ptr::{NonNull, with_exposed_provenance_mut};

/// `SpscBuffer` requires us to specify how the underlying memory is managed -- it abstracts over a
/// buffer provided by userspace or the kernel. Since we're writing to buffers in the kernel, we
/// provide an allocator backed by a `VmAspace`.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelAspaceAllocator;

impl KernelAspaceAllocator {
    /// Allocates and maps a pinned region of `size` bytes (rounded up to a page multiple) in the
    /// kernel address space.
    pub fn allocate(size: u32) -> Option<NonNull<u8>> {
        let rounded_size = page::round_up(size as usize);
        if size == 0 {
            return None;
        }
        let Ok(vmo) =
            VmObjectPaged::create(ALLOC_FLAG_ANY | ALLOC_FLAG_CAN_WAIT, 0, rounded_size as u64)
        else {
            return None;
        };
        if vmo.commit_range_pinned(0, rounded_size as u64, true).is_err() {
            return None;
        }
        let kaspace = VmAspace::kernel_aspace();

        let Ok(r) = kaspace
            .root_vmar()
            .expect("kernel aspace root VMAR must be initialized")
            .create_vm_mapping_no_overwrite(
                0,
                rounded_size,
                0,
                0,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            )
        else {
            vmo.unpin(0, rounded_size as u64);
            return None;
        };
        if r.mapping.map_range(0, rounded_size, true, true).is_err() {
            let _ = r.mapping.destroy();
            vmo.unpin(0, rounded_size as u64);
            return None;
        }
        NonNull::new(with_exposed_provenance_mut(r.base))
    }

    /// Frees a kernel address space region previously allocated by [`Self::allocate`].
    ///
    /// # Safety
    ///
    /// If `ptr` is `Some`, it is the base address of a region allocated by [`Self::allocate`] in
    /// the kernel address space and not freed since, and nothing uses that region any more.
    pub unsafe fn free(ptr: Option<NonNull<u8>>) {
        if let Some(ptr) = ptr {
            let kaspace = VmAspace::kernel_aspace();
            // SAFETY: `ptr` is in a mapping of the root VMAR whose whole VMO range is pinned by a
            // pin this code owns, and nothing uses that mapping any more.
            let _ = unsafe { kaspace.free_region(ptr.addr().get()) };
        }
    }
}
