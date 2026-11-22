// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Views of the kernel counter VMOs published in `/boot/kernel`.
//!
//! See `counters_abi` for the layout of the VMOs.

use counters_abi::{ARENA_VMO_NAME, DESCRIPTOR_VMO_NAME, Descriptor, DescriptorVmo};
use std::mem::{align_of, size_of};
use std::num::NonZero;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, Ordering};
use zerocopy::{FromZeros, IntoBytes};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("8-byte Relaxed atomic loads on read-only mappings require a 64-bit target");

const _: () = assert!(
    size_of::<AtomicI64>() == size_of::<i64>() && align_of::<AtomicI64>() == align_of::<i64>()
);

/// Converts a VMO name from the ABI to a `&str` at compile time.
const fn vmo_name(name: &'static [u8]) -> &'static str {
    match core::str::from_utf8(name) {
        Ok(name) => name,
        Err(_) => panic!("VMO name is not UTF-8"),
    }
}

/// Name of the descriptor VMO file in `/boot/kernel` (`DescriptorVmo::VMO_NAME`).
pub const DESCRIPTOR_VMO_FILE: &str = vmo_name(DESCRIPTOR_VMO_NAME);

/// Name of the arena VMO file in `/boot/kernel` (`ARENA_VMO_NAME`).
pub const ARENA_VMO_FILE: &str = vmo_name(ARENA_VMO_NAME);

/// Reasons the descriptor VMO is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorsError {
    /// Reading the VMO failed.
    Read(zx::Status),
    /// The header's magic number is not `DescriptorVmo::MAGIC`.
    BadMagic {
        /// The magic number found in the VMO.
        magic: u64,
    },
    /// The VMO is too small to hold the descriptor table it claims to have.
    TooSmall {
        /// The size of the VMO in bytes.
        size: u64,
        /// The `descriptor_table_size` from the header.
        descriptor_table_size: u64,
    },
}

/// A validated copy of the descriptor VMO: the header and its descriptor
/// table, which is sorted by name.
#[derive(Debug)]
pub struct Descriptors {
    header: DescriptorVmo,
    table: Vec<Descriptor>,
}

impl Descriptors {
    /// Reads the header and descriptor table out of a descriptor VMO of `size`
    /// bytes, checking its magic number and size.
    pub fn from_vmo(vmo: &zx::Vmo, size: u64) -> Result<Self, DescriptorsError> {
        if size < size_of::<DescriptorVmo>() as u64 {
            return Err(DescriptorsError::TooSmall { size, descriptor_table_size: 0 });
        }
        let mut header = DescriptorVmo::new_zeroed();
        vmo.read(header.as_mut_bytes(), 0).map_err(DescriptorsError::Read)?;
        if header.magic != DescriptorVmo::MAGIC {
            return Err(DescriptorsError::BadMagic { magic: header.magic });
        }
        let descriptor_table_size = header.descriptor_table_size;
        let needed = (size_of::<DescriptorVmo>() as u64).checked_add(descriptor_table_size);
        if needed.is_none_or(|needed| size < needed) {
            return Err(DescriptorsError::TooSmall { size, descriptor_table_size });
        }
        let Ok(table_bytes) = usize::try_from(descriptor_table_size) else {
            return Err(DescriptorsError::TooSmall { size, descriptor_table_size });
        };
        let mut table = vec![Descriptor::new_zeroed(); table_bytes / size_of::<Descriptor>()];
        vmo.read(table.as_mut_bytes(), size_of::<DescriptorVmo>() as u64)
            .map_err(DescriptorsError::Read)?;
        Ok(Self { header, table })
    }

    /// Returns the VMO header.
    pub fn header(&self) -> &DescriptorVmo {
        &self.header
    }

    /// Returns the descriptor table, which is sorted by name.
    pub fn table(&self) -> &[Descriptor] {
        &self.table
    }

    /// Returns the number of counters in the table.
    pub fn num_counters(&self) -> usize {
        self.table.len()
    }

    /// Returns the minimum size in bytes of the arena VMO, or `None` if it
    /// overflows.
    pub fn arena_size(&self) -> Option<u64> {
        self.header
            .max_cpus
            .checked_mul(self.num_counters() as u64)?
            .checked_mul(size_of::<i64>() as u64)
    }
}

/// Reasons the arena VMO cannot be mapped or used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArenaError {
    /// The VMO is smaller than [`Descriptors::arena_size()`], or that size overflows.
    TooSmall,
    /// Mapping the VMO into the root VMAR failed.
    Map(zx::Status),
}

/// A read-only mapping of the kernel counter arena VMO: `max_cpus` arrays of
/// `num_counters` 64-bit values, each indexed by the index into the descriptor
/// table.
#[derive(Debug)]
pub struct Arena {
    // Safety invariant: `Some(addr)` iff `mapped_size > 0`. When `Some(addr)`,
    // `addr` is page-aligned (hence aligned to `AtomicI64`) and points to an
    // active read-only, paged, non-resizable, non-discardable,
    // non-pager-backed, `Cached` root-VMAR mapping of `mapped_size`
    // page-aligned bytes (`<= isize::MAX as usize`), valid for `AtomicI64`
    // `Relaxed` loads until `Drop` unmaps it.
    addr: Option<NonNull<AtomicI64>>,
    // Safety invariant: `0` when `addr` is `None`; otherwise a non-zero
    // multiple of the system page size satisfying
    // `max_cpus * num_counters * size_of::<AtomicI64>() <= mapped_size <= isize::MAX as usize`.
    mapped_size: usize,
    // Safety invariant: `max_cpus * num_counters * size_of::<AtomicI64>()` does
    // not overflow and is `<= mapped_size`.
    max_cpus: u64,
    num_counters: usize,
}

impl Arena {
    /// Maps the `desc.arena_size()`-byte prefix of `vmo` (cross-checked against
    /// `size` and `vmo.info()`) read-only into the root VMAR and returns an
    /// [`Arena`] view validated against `desc`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// 1. During this call and for the lifetime of the returned [`Arena`], the
    ///    first `desc.arena_size()` bytes of `vmo` remain immutable or are
    ///    mutated concurrently only via aligned 64-bit atomic operations (such
    ///    as the Zircon kernel's `counters::Counter` updates on
    ///    `/boot/kernel/counters/arena`). Non-atomic concurrent writes (for
    ///    example through a writable mapping, `zx_vmo_write`, or
    ///    `zx_vmo_transfer_data`), page-replacing VMO range operations (such as
    ///    `ZX_VMO_OP_DECOMMIT` or `ZX_VMO_OP_ZERO` via `zx_vmo_op_range` or
    ///    `zx_vmar_op_range`) overlapping the mapped pages, or concurrent
    ///    changes to `vmo`'s cache policy during `Arena::map` must not occur.
    /// 2. For the lifetime of the returned [`Arena`], nothing outside
    ///    `Arena::drop` unmaps, overwrites (`ZX_VM_SPECIFIC_OVERWRITE`), or
    ///    removes read permissions (`zx_vmar_protect`) on the mapping created
    ///    in `fuchsia_runtime::vmar_root_self()`.
    pub unsafe fn map(vmo: &zx::Vmo, size: u64, desc: &Descriptors) -> Result<Self, ArenaError> {
        let Some(needed) = desc.arena_size() else {
            return Err(ArenaError::TooSmall);
        };
        if size < needed {
            return Err(ArenaError::TooSmall);
        }
        let info = vmo.info().map_err(ArenaError::Map)?;
        if info.size_bytes < needed {
            return Err(ArenaError::TooSmall);
        }
        if !info.flags.contains(zx::VmoInfoFlags::PAGED)
            || info.flags.intersects(
                zx::VmoInfoFlags::RESIZABLE
                    | zx::VmoInfoFlags::DISCARDABLE
                    | zx::VmoInfoFlags::PAGER_BACKED,
            )
            || info.cache_policy() != zx::CachePolicy::Cached
        {
            return Err(ArenaError::Map(zx::Status::NOT_SUPPORTED));
        }
        let (addr, mapped_size) = if needed == 0 {
            (None, 0)
        } else {
            let needed_usize =
                usize::try_from(needed).map_err(|_| ArenaError::Map(zx::Status::OUT_OF_RANGE))?;
            let page_size = zx::system_get_page_size() as usize;
            let mapped_size = needed_usize
                .checked_next_multiple_of(page_size)
                .ok_or(ArenaError::Map(zx::Status::OUT_OF_RANGE))?;
            if (mapped_size as u64) > info.size_bytes {
                return Err(ArenaError::TooSmall);
            }
            if isize::try_from(mapped_size).is_err() {
                return Err(ArenaError::Map(zx::Status::OUT_OF_RANGE));
            }
            let raw_addr = fuchsia_runtime::vmar_root_self()
                .map(
                    0,
                    vmo,
                    0,
                    mapped_size,
                    zx::VmarFlags::PERM_READ | zx::VmarFlags::REQUIRE_NON_RESIZABLE,
                )
                .map_err(ArenaError::Map)?;
            debug_assert_eq!(raw_addr % page_size, 0);
            debug_assert_eq!(raw_addr % align_of::<AtomicI64>(), 0);
            let addr = NonZero::new(raw_addr)
                .map(NonNull::<AtomicI64>::with_exposed_provenance)
                .ok_or(ArenaError::Map(zx::Status::INTERNAL))?;
            (Some(addr), mapped_size)
        };
        Ok(Self {
            addr,
            mapped_size,
            max_cpus: desc.header().max_cpus,
            num_counters: desc.num_counters(),
        })
    }

    /// Reads the current value of the counter at index `idx` of the
    /// descriptor table for CPU `cpu`.
    ///
    /// # Panics
    ///
    /// Panics if `cpu` or `idx` is out of range.
    pub fn get(&self, cpu: u64, idx: usize) -> i64 {
        assert!(cpu < self.max_cpus, "CPU {cpu} out of range");
        assert!(idx < self.num_counters, "counter index {idx} out of range");
        let offset = cpu as usize * self.num_counters + idx;
        let ptr = self.addr.expect("non-empty arena has a mapping").as_ptr();
        // SAFETY:
        // - `cpu < self.max_cpus` and `idx < self.num_counters` imply `self.mapped_size > 0`,
        //   so by `Arena`'s field invariants and `Arena::map`'s `# Safety` contract, `ptr`
        //   is non-null, page-aligned (aligned to `align_of::<AtomicI64>() == 8`), and points
        //   to a live read-only, paged, non-resizable, non-discardable, non-pager-backed,
        //   `Cached` mapping of `self.mapped_size` bytes (`<= isize::MAX as usize`) until
        //   `Self::drop` unmaps it.
        // - `offset = cpu * self.num_counters + idx < self.max_cpus * self.num_counters`, and
        //   `self.max_cpus * self.num_counters * size_of::<AtomicI64>() <= self.mapped_size`,
        //   so `ptr.add(offset)` is in bounds of the mapped allocation and cannot overflow
        //   `isize`.
        // - By `Arena::map`'s `# Safety` precondition, the mapped pages are not decommitted or
        //   zeroed via VMO range ops and any concurrent mutations to this slot use aligned
        //   64-bit atomic operations; `AtomicI64` provides interior mutability (`!Freeze`).
        // - Per `core::sync::atomic` ("Atomic accesses to read-only memory"), 8-byte
        //   `Ordering::Relaxed` loads are valid on read-only cached mappings on `x86_64`,
        //   `aarch64`, and `riscv64` (enforced by the `compile_error!` guard above).
        let slot = unsafe { &*ptr.add(offset) };
        // No `Ordering::Acquire` fence is needed: each counter slot is a standalone
        // diagnostic integer updated by the kernel with `Ordering::Relaxed` stores,
        // with no `Release`-synchronized dependent reads.
        slot.load(Ordering::Relaxed)
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        if let Some(addr) = self.addr {
            // SAFETY: By `Arena`'s field invariants and `Arena::map`'s `# Safety`
            // contract, `addr` and `self.mapped_size` describe the active,
            // non-zero, page-aligned read-only data mapping created in
            // `vmar_root_self()` by `Arena::map` (and untouched by external VMAR
            // operations). Holding `&mut self` in `drop` guarantees no references
            // into the mapping remain live, and the mapping contains no
            // executable code.
            unsafe {
                let _ =
                    fuchsia_runtime::vmar_root_self().unmap(addr.addr().get(), self.mapped_size);
            }
        }
    }
}
