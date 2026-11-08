// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use ebpf_core::maps::*;

use static_assertions::const_assert_eq;

const_assert_eq!(MapFlags::NoPrealloc.bits(), linux_uapi::BPF_F_NO_PREALLOC);
const_assert_eq!(MapFlags::NoCommonLru.bits(), linux_uapi::BPF_F_NO_COMMON_LRU);
const_assert_eq!(MapFlags::NumaNode.bits(), linux_uapi::BPF_F_NUMA_NODE);
const_assert_eq!(MapFlags::SyscallReadOnly.bits(), linux_uapi::BPF_F_RDONLY);
const_assert_eq!(MapFlags::SyscallWriteOnly.bits(), linux_uapi::BPF_F_WRONLY);
const_assert_eq!(MapFlags::StackBuildId.bits(), linux_uapi::BPF_F_STACK_BUILD_ID);
const_assert_eq!(MapFlags::ZeroSeed.bits(), linux_uapi::BPF_F_ZERO_SEED);
const_assert_eq!(MapFlags::ProgReadOnly.bits(), linux_uapi::BPF_F_RDONLY_PROG);
const_assert_eq!(MapFlags::ProgWriteOnly.bits(), linux_uapi::BPF_F_WRONLY_PROG);
const_assert_eq!(MapFlags::Clone.bits(), linux_uapi::BPF_F_CLONE);
const_assert_eq!(MapFlags::Mmapable.bits(), linux_uapi::BPF_F_MMAPABLE);
const_assert_eq!(MapFlags::PreserveElems.bits(), linux_uapi::BPF_F_PRESERVE_ELEMS);
const_assert_eq!(MapFlags::InnerMap.bits(), linux_uapi::BPF_F_INNER_MAP);
const_assert_eq!(MapFlags::Link.bits(), linux_uapi::BPF_F_LINK);
const_assert_eq!(MapFlags::PathFd.bits(), linux_uapi::BPF_F_PATH_FD);
const_assert_eq!(MapFlags::VtypeBtfObjFd.bits(), linux_uapi::BPF_F_VTYPE_BTF_OBJ_FD);
const_assert_eq!(MapFlags::TokenFd.bits(), linux_uapi::BPF_F_TOKEN_FD);
const_assert_eq!(MapFlags::SegvOnFault.bits(), linux_uapi::BPF_F_SEGV_ON_FAULT);
const_assert_eq!(MapFlags::NoUserConv.bits(), linux_uapi::BPF_F_NO_USER_CONV);
const_assert_eq!(MapFlags::all().bits(), (1 << 19) - 1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_flags_operations() {
        assert_eq!(MapFlags::empty().bits(), 0);
        assert!(MapFlags::empty().is_empty());
        assert!(MapFlags::all().is_all());

        // Unknown bit 19 is rejected by `from_bits`, masked by `from_bits_truncate`, and kept by
        // `from_bits_retain`.
        let unknown_bit = 1u32 << 19;
        assert_eq!(MapFlags::from_bits(unknown_bit), None);
        assert_eq!(
            MapFlags::from_bits(MapFlags::NoPrealloc.bits() | MapFlags::Clone.bits()),
            Some(MapFlags::NoPrealloc | MapFlags::Clone)
        );
        assert_eq!(
            MapFlags::from_bits_truncate(unknown_bit | MapFlags::ProgReadOnly.bits()),
            MapFlags::ProgReadOnly
        );
        let retained = MapFlags::from_bits_retain(unknown_bit | MapFlags::all().bits());
        assert_eq!(retained.bits(), unknown_bit | MapFlags::all().bits());
        assert!(retained.is_all());

        // Set operations and bitwise `!` mask out bits outside `ALL_BITS`.
        let mut flags = MapFlags::NoPrealloc | MapFlags::ProgReadOnly;
        assert!(flags.contains(MapFlags::NoPrealloc));
        assert!(flags.intersects(MapFlags::ProgReadOnly | MapFlags::Clone));
        assert!(!flags.contains(MapFlags::ProgReadOnly | MapFlags::Clone));
        flags.remove(MapFlags::NoPrealloc);
        assert_eq!(flags, MapFlags::ProgReadOnly);

        let inv = !MapFlags::NoPrealloc;
        assert_eq!(inv, MapFlags::NoPrealloc.complement());
        assert_eq!(inv.bits() & !MapFlags::all().bits(), 0);
        assert!(!inv.contains(MapFlags::NoPrealloc));
        assert!(inv.contains(MapFlags::NoUserConv));
    }
}
