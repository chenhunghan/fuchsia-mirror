// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

//! This module describes how the kernel exposes its internal counters to userland.
//! This is a PRIVATE UNSTABLE ABI that may change at any time! The layouts used
//! here; the set of counters; their names, meanings, and types; and the set of
//! available types; are all subject to change in every kernel version and are
//! not meant to be any kind of stable ABI between the kernel and userland.
//!
//! The expectation is that these layouts will be used only by a single
//! privileged service that is tightly-coupled with the kernel, i.e. always built
//! from source when building the kernel.
//!
//! The counters exist only for kernel-specific diagnostic and logging purposes.

use core::mem::{align_of, offset_of, size_of};
use counters_bindings as bindings;

/// The aggregation type of a kernel counter.
///
/// This specifies how the diagnostic tools should combine the per-CPU slot values
/// of the counter to produce a single diagnostic value.
#[repr(u64)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Type {
    /// Padding element (unused).
    Padding = 0,
    /// Standard summation counter (aggregates the sum across all CPUs).
    Sum = 1,
    /// Minimum tracker counter (finds the minimum value across all CPUs).
    Min = 2,
    /// Maximum tracker counter (finds the maximum value across all CPUs).
    Max = 3,
}

zr::static_assert!(Type::Padding as u64 == bindings::counters_Type_kPadding);
zr::static_assert!(Type::Sum as u64 == bindings::counters_Type_kSum);
zr::static_assert!(Type::Min as u64 == bindings::counters_Type_kMin);
zr::static_assert!(Type::Max as u64 == bindings::counters_Type_kMax);

/// Binary-stable C-compatible representation of a kernel counter descriptor.
///
/// The memory layout of this structure matches Zircon's `counters::Descriptor` exactly,
/// enabling the linker and userspace diagnostic tools to parse Rust-declared counters
/// seamlessly from the kernel's binary segments.
#[repr(C, align(8))]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Descriptor {
    pub name: [u8; 56],
    pub type_: u64,
}

// kernel.ld uses this size to ASSERT that enough space has been reserved in the counters arena.
zr::static_assert!(size_of::<Descriptor>() == 64);
// kernel.ld knows there is no alignment padding between the VMO header and the descriptor table.
zr::static_assert!(align_of::<Descriptor>() == 8);
zr::static_assert_size_and_align!(
    Descriptor,
    size_of::<bindings::counters_Descriptor>(),
    align_of::<bindings::counters_Descriptor>(),
);
zr::static_assert!(offset_of!(Descriptor, name) == offset_of!(bindings::counters_Descriptor, name));
zr::static_assert!(
    offset_of!(Descriptor, type_) == offset_of!(bindings::counters_Descriptor, type_)
);

impl Descriptor {
    /// Create a new raw `Descriptor` instance with the given packed name and type value.
    pub const fn new(name: [u8; 56], type_: u64) -> Self {
        Self { name, type_ }
    }
}

/// Header layout of the kernel counter descriptor VMO (`counters::DescriptorVmo`).
///
/// Followed in the VMO by `num_counters()` entries of [`Descriptor`], sorted by name.
/// Each index into that table corresponds to an index into a per-CPU array in the arena.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DescriptorVmo {
    /// Magic number identifying the `DescriptorVmo` layout (`DescriptorVmo::MAGIC`).
    pub magic: u64,
    /// Maximum number of CPUs (`SMP_MAX_CPUS`).
    pub max_cpus: u64,
    /// Size in bytes of the trailing descriptor table (`sizeof(descriptor_table)`).
    pub descriptor_table_size: u64,
}

impl DescriptorVmo {
    /// `PA_VMO_KERNEL_FILE` with this name has the `DescriptorVmo` layout.
    pub const VMO_NAME: &[u8] = b"counters/desc";

    /// This is `time_t` as of writing. Change it when changing this layout.
    // TODO(mcgrathr): Maybe generate these uniquely at build time from
    // the kernel version info or something?
    pub const MAGIC: u64 = 1547273975;

    /// Returns the number of [`Descriptor`] entries in the descriptor table.
    pub const fn num_counters(&self) -> usize {
        (self.descriptor_table_size as usize) / size_of::<Descriptor>()
    }
}

// kernel.ld knows the layout of DescriptorVmo.
zr::static_assert!(
    offset_of!(DescriptorVmo, descriptor_table_size) == 16 && size_of::<DescriptorVmo>() == 24
);
zr::static_assert_size_and_align!(
    DescriptorVmo,
    size_of::<bindings::counters_DescriptorVmo>(),
    align_of::<bindings::counters_DescriptorVmo>(),
);
zr::static_assert!(
    offset_of!(DescriptorVmo, magic) == offset_of!(bindings::counters_DescriptorVmo, magic)
);
zr::static_assert!(
    offset_of!(DescriptorVmo, max_cpus) == offset_of!(bindings::counters_DescriptorVmo, max_cpus)
);
zr::static_assert!(
    offset_of!(DescriptorVmo, descriptor_table_size)
        == offset_of!(bindings::counters_DescriptorVmo, descriptor_table_size)
);
zr::static_assert!(
    size_of::<DescriptorVmo>() == offset_of!(bindings::counters_DescriptorVmo, descriptor_table)
);
zr::static_assert!(DescriptorVmo::MAGIC == bindings::counters_DescriptorVmo_kMagic);

/// Name of the kernel counter descriptor VMO (`counters::DescriptorVmo::kVmoName`).
pub const DESCRIPTOR_VMO_NAME: &[u8] = DescriptorVmo::VMO_NAME;

/// `PA_VMO_KERNEL_FILE` with this name holds an array of `SMP_MAX_CPUS`
/// arrays, each of which is `i64[num_counters()]` indexed by the
/// index into `DescriptorVmo`'s descriptor table (`counters::kArenaVmoName`).
pub const ARENA_VMO_NAME: &[u8] = b"counters/arena";
