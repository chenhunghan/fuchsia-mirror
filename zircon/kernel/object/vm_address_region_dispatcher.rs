// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_address_region_dispatcher_ffi::cpp_vmar_dispatcher_create;
use crate::counters::define_kcounter;
use crate::kernel::types::VAddr;
use crate::user_copy::UserInOutPtr;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ArchMmuFlags,
};
pub use crate::vm::vm_address_region::MemoryPriority;
use crate::vm::vm_address_region::{
    MapResult, RangeOpType, VmAddressRegion, VmAddressRegionOpChildren, flag as vmar_flag,
};
use crate::vm::vm_object::VmObject;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_VMAR_RIGHTS, ZX_OBJ_TYPE_VMAR, ZX_RIGHT_EXECUTE, ZX_RIGHT_OP_CHILDREN,
    ZX_RIGHT_READ, ZX_RIGHT_WRITE, ZX_VM_ALIGN_BASE, ZX_VM_ALLOW_FAULTS, ZX_VM_CAN_MAP_EXECUTE,
    ZX_VM_CAN_MAP_READ, ZX_VM_CAN_MAP_SPECIFIC, ZX_VM_CAN_MAP_WRITE, ZX_VM_COMPACT,
    ZX_VM_FAULT_BEYOND_STREAM_SIZE, ZX_VM_OFFSET_IS_UPPER_LIMIT, ZX_VM_PERM_EXECUTE,
    ZX_VM_PERM_READ, ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED, ZX_VM_PERM_WRITE,
    ZX_VM_REQUIRE_NON_RESIZABLE, ZX_VM_SPECIFIC, ZX_VM_SPECIFIC_OVERWRITE, ZX_VMAR_OP_ALWAYS_NEED,
    ZX_VMAR_OP_COMMIT, ZX_VMAR_OP_DECOMMIT, ZX_VMAR_OP_DONT_NEED, ZX_VMAR_OP_MAP_RANGE,
    ZX_VMAR_OP_PREFETCH, ZX_VMAR_OP_ZERO, zx_info_vmar_t, zx_rights_t,
};

zr::static_assert_size_and_align!(
    VmAddressRegionDispatcherState,
    object_constants::kVmAddressRegionDispatcherStateSize,
    object_constants::kVmAddressRegionDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_VMAR_CREATE_COUNT, "dispatcher.vmar.create", Sum);
define_kcounter!(DISPATCHER_VMAR_DESTROY_COUNT, "dispatcher.vmar.destroy", Sum);

#[inline]
fn extract_flag<T: Default>(flags: &mut u32, from_flag: u32, to_flag: T) -> T {
    let flag_set = (*flags & from_flag) != 0;
    // Unconditionally clear `flags` so that the compiler can more easily see that multiple
    // `extract_flag` invocations can just use a single combined clear, greatly reducing code-gen.
    *flags &= !from_flag;
    if flag_set { to_flag } else { T::default() }
}

// Split out the syscall flags into vmar flags and mmu flags.  Note that this
// does not validate that the requested protections in *flags* are valid.  For
// that use is_valid_mapping_protection()
fn split_syscall_flags(
    mut flags: u32,
    base_arch_mmu_flags: ArchMmuFlags,
) -> Result<(u32, ArchMmuFlags, u8), Status> {
    // Figure out arch_mmu_flags
    let mut mmu_flags: ArchMmuFlags = 0;
    mmu_flags |= extract_flag(&mut flags, ZX_VM_PERM_READ, ARCH_MMU_FLAG_PERM_READ);
    mmu_flags |= extract_flag(&mut flags, ZX_VM_PERM_WRITE, ARCH_MMU_FLAG_PERM_WRITE);
    mmu_flags |= extract_flag(&mut flags, ZX_VM_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_EXECUTE);

    // This flag is no longer needed and should have already been acted upon.
    let _: u32 = extract_flag(&mut flags, ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED, 0);

    // Figure out vmar flags
    let mut vmar: u32 = 0;
    vmar |= extract_flag(&mut flags, ZX_VM_COMPACT, vmar_flag::COMPACT);
    vmar |= extract_flag(&mut flags, ZX_VM_SPECIFIC, vmar_flag::SPECIFIC);
    vmar |= extract_flag(&mut flags, ZX_VM_SPECIFIC_OVERWRITE, vmar_flag::SPECIFIC_OVERWRITE);
    vmar |= extract_flag(&mut flags, ZX_VM_CAN_MAP_SPECIFIC, vmar_flag::CAN_MAP_SPECIFIC);
    vmar |= extract_flag(&mut flags, ZX_VM_CAN_MAP_READ, vmar_flag::CAN_MAP_READ);
    vmar |= extract_flag(&mut flags, ZX_VM_CAN_MAP_WRITE, vmar_flag::CAN_MAP_WRITE);
    vmar |= extract_flag(&mut flags, ZX_VM_CAN_MAP_EXECUTE, vmar_flag::CAN_MAP_EXECUTE);
    vmar |= extract_flag(&mut flags, ZX_VM_REQUIRE_NON_RESIZABLE, vmar_flag::REQUIRE_NON_RESIZABLE);
    vmar |= extract_flag(&mut flags, ZX_VM_ALLOW_FAULTS, vmar_flag::ALLOW_FAULTS);
    vmar |= extract_flag(&mut flags, ZX_VM_OFFSET_IS_UPPER_LIMIT, vmar_flag::OFFSET_IS_UPPER_LIMIT);
    vmar |= extract_flag(
        &mut flags,
        ZX_VM_FAULT_BEYOND_STREAM_SIZE,
        vmar_flag::FAULT_BEYOND_STREAM_SIZE,
    );

    if (flags & ((1u32 << ZX_VM_ALIGN_BASE) - 1)) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // Figure out alignment.
    let alignment = (flags >> ZX_VM_ALIGN_BASE) as u8;

    if ((alignment < 10) && (alignment != 0)) || (alignment > 32) {
        return Err(Status::INVALID_ARGS);
    }

    Ok((vmar, base_arch_mmu_flags | mmu_flags, alignment))
}

/// Internal state storage for `VmAddressRegionDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmAddressRegionDispatcherState {
    canary: Canary<{ fbl::magic(b"VARD") }>,
    vmar: RefPtr<VmAddressRegion>,
    base_arch_mmu_flags: ArchMmuFlags,
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl VmAddressRegionDispatcherState {
    /// Initializes the `VmAddressRegionDispatcherState`.
    pub fn init(
        _dispatcher: *const VmAddressRegionDispatcher,
        vmar: RefPtr<VmAddressRegion>,
        base_arch_mmu_flags: ArchMmuFlags,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            canary: {
                DISPATCHER_VMAR_CREATE_COUNT.add(1);
                Canary::new()
            },
            vmar,
            base_arch_mmu_flags,
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for VmAddressRegionDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_VMAR_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct VmAddressRegionDispatcher,
    VmAddressRegionDispatcherState,
    ZX_OBJ_TYPE_VMAR,
    object_constants::kVmAddressRegionDispatcherStateOffset
);

impl VmAddressRegionDispatcher {
    /// Returns the default rights for a `VmAddressRegionDispatcher` handle.
    pub const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_VMAR_RIGHTS
    }

    /// Creates a new `VmAddressRegionDispatcher` wrapping `vmar`.
    pub fn create(
        vmar: RefPtr<VmAddressRegion>,
        base_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(KernelHandle<VmAddressRegionDispatcher>, zx_rights_t), Status> {
        // The initial rights should match the VMAR's creation permissions
        let mut vmar_rights = Self::default_rights();
        let vmar_flags = vmar.flags();
        if (vmar_flags & vmar_flag::CAN_MAP_READ) != 0 {
            vmar_rights |= ZX_RIGHT_READ;
        }
        if (vmar_flags & vmar_flag::CAN_MAP_WRITE) != 0 {
            vmar_rights |= ZX_RIGHT_WRITE;
        }
        if (vmar_flags & vmar_flag::CAN_MAP_EXECUTE) != 0 {
            vmar_rights |= ZX_RIGHT_EXECUTE;
        }

        // SAFETY: `RefPtr::into_raw(vmar)` transfers ownership of `vmar` to
        // `cpp_vmar_dispatcher_create`, which adopts it into `fbl::RefPtr<VmAddressRegion>` and
        // initializes `out` on `ZX_OK`.
        let new_handle = unsafe {
            KernelHandle::create(|out| {
                cpp_vmar_dispatcher_create(
                    RefPtr::into_raw(vmar).cast_mut(),
                    base_arch_mmu_flags,
                    out,
                )
            })
        }?;

        Ok((new_handle, vmar_rights))
    }

    /// Returns a reference to the underlying `VmAddressRegion`.
    pub fn vmar(&self) -> &RefPtr<VmAddressRegion> {
        &self.state().vmar
    }

    // TODO(teisenbe): Make this the planned batch interface
    /// Allocates a sub-VMAR within this VMAR.
    pub fn allocate(
        &self,
        offset: usize,
        size: usize,
        flags: u32,
    ) -> Result<(KernelHandle<VmAddressRegionDispatcher>, zx_rights_t), Status> {
        let state = self.state();
        state.canary.assert();

        let (vmar_flags, arch_mmu_flags, alignment) = split_syscall_flags(flags, 0)?;

        // Check if any MMU-related flags were requested.
        if arch_mmu_flags != 0 {
            return Err(Status::INVALID_ARGS);
        }

        let new_vmar =
            state.vmar.create_sub_vmar(offset, size, alignment, vmar_flags, b"useralloc")?;

        Self::create(new_vmar, state.base_arch_mmu_flags)
    }

    /// Destroys the underlying `VmAddressRegion`.
    pub fn destroy(&self) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        let vmar = &state.vmar;
        // Disallow destroying the root vmar of an aspace as this violates the aspace invariants.
        if vmar.aspace().root_vmar().is_some_and(|root| core::ptr::eq(&*root, &**vmar)) {
            return Err(Status::NOT_SUPPORTED);
        }

        vmar.destroy()
    }

    /// Maps a VMO into this VMAR.
    pub fn map(
        &self,
        vmar_offset: usize,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        len: usize,
        flags: u32,
    ) -> Result<MapResult, Status> {
        let state = self.state();
        state.canary.assert();

        if !Self::is_valid_mapping_protection(flags) {
            return Err(Status::INVALID_ARGS);
        }

        // Split flags into vmar_flags and arch_mmu_flags
        let (mut vmar_flags, arch_mmu_flags, alignment) =
            split_syscall_flags(flags, state.base_arch_mmu_flags)?;

        if (vmar_flags & vmar_flag::REQUIRE_NON_RESIZABLE) != 0 {
            vmar_flags &= !vmar_flag::REQUIRE_NON_RESIZABLE;
            if vmo.is_resizable() {
                return Err(Status::NOT_SUPPORTED);
            }
        }

        if (vmar_flags & vmar_flag::ALLOW_FAULTS) != 0 {
            vmar_flags &= !vmar_flag::ALLOW_FAULTS;
        } else {
            // TODO(https://fxbug.dev/42109795): Add additional checks once all clients (resizable
            // and pager-backed VMOs) start using the vmar_flag::ALLOW_FAULTS flag.
            if vmo.is_discardable() {
                return Err(Status::NOT_SUPPORTED);
            }
        }

        state.vmar.create_vm_mapping(
            vmar_offset,
            len,
            alignment,
            vmar_flags,
            vmo,
            vmo_offset,
            arch_mmu_flags,
            b"useralloc",
        )
    }

    /// Changes protections on a subset of the region of memory in the containing address space.
    pub fn protect(
        &self,
        base: VAddr,
        len: usize,
        flags: u32,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        if !page::is_aligned(base.0) {
            return Err(Status::INVALID_ARGS);
        }

        if !Self::is_valid_mapping_protection(flags) {
            return Err(Status::INVALID_ARGS);
        }

        let (vmar_flags, arch_mmu_flags, alignment) =
            split_syscall_flags(flags, state.base_arch_mmu_flags)?;

        // This request does not allow any VMAR flags or alignment flags to be set.
        if vmar_flags != 0 || alignment != 0 {
            return Err(Status::INVALID_ARGS);
        }

        state.vmar.protect(base, len, arch_mmu_flags, op_children)
    }

    /// Converts a syscall `ZX_VMAR_OP_*` code to a `RangeOpType`.
    pub fn range_op_type_from_code(op: u32) -> Option<RangeOpType> {
        match op {
            ZX_VMAR_OP_COMMIT => Some(RangeOpType::Commit),
            ZX_VMAR_OP_DECOMMIT => Some(RangeOpType::Decommit),
            ZX_VMAR_OP_MAP_RANGE => Some(RangeOpType::MapRange),
            ZX_VMAR_OP_ZERO => Some(RangeOpType::Zero),
            ZX_VMAR_OP_DONT_NEED => Some(RangeOpType::DontNeed),
            ZX_VMAR_OP_ALWAYS_NEED => Some(RangeOpType::AlwaysNeed),
            ZX_VMAR_OP_PREFETCH => Some(RangeOpType::Prefetch),
            _ => None,
        }
    }

    /// Returns whether `rights` permits performing `op` on a VMAR.
    pub fn is_operation_allowed_from_rights(op: RangeOpType, rights: zx_rights_t) -> bool {
        match op {
            RangeOpType::Commit | RangeOpType::Decommit | RangeOpType::Zero => {
                (rights & ZX_RIGHT_WRITE) != 0
            }
            RangeOpType::Prefetch | RangeOpType::MapRange => (rights & ZX_RIGHT_READ) != 0,
            RangeOpType::DontNeed | RangeOpType::AlwaysNeed => true, // just hints
        }
    }

    /// Performs a range operation across mappings in `[base, base + len)`.
    pub fn range_op(
        &self,
        op: u32,
        base: VAddr,
        len: usize,
        rights: zx_rights_t,
        buffer: UserInOutPtr<u8>,
        buffer_size: usize,
    ) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        let Some(which_op) = Self::range_op_type_from_code(op) else {
            return Err(Status::INVALID_ARGS);
        };

        if !Self::is_operation_allowed_from_rights(which_op, rights) {
            return Err(Status::ACCESS_DENIED);
        }

        let op_children = Self::op_children_from_rights(rights);
        state.vmar.range_op(which_op, base, len, op_children, buffer, buffer_size)
    }

    /// Unmaps a range of virtual addresses within this VMAR.
    pub fn unmap(
        &self,
        base: VAddr,
        len: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        if !page::is_aligned(base.0) {
            return Err(Status::INVALID_ARGS);
        }

        // SAFETY: `VmAddressRegionDispatcher` only wraps user or guest address regions, so
        // unmapping addresses within it cannot invalidate active kernel memory references.
        unsafe { state.vmar.unmap(base, len, op_children) }
    }

    /// Sets memory priority for this VMAR dispatcher.
    pub fn set_memory_priority(&self, memory_priority: MemoryPriority) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        state.vmar.set_memory_priority(memory_priority)
    }

    /// Returns information about this VMAR.
    pub fn get_vmar_info(&self) -> zx_info_vmar_t {
        let vmar = self.vmar();
        zx_info_vmar_t { base: vmar.base().0, len: vmar.size() }
    }

    // Check if the given flags define an allowed combination of RWX
    // protections.
    pub fn is_valid_mapping_protection(flags: u32) -> bool {
        if (flags & ZX_VM_PERM_READ) == 0 {
            // No way to express non-readable mappings that are also writeable or
            // executable.
            if (flags & (ZX_VM_PERM_WRITE | ZX_VM_PERM_EXECUTE)) != 0 {
                return false;
            }
        }
        true
    }

    /// Returns whether child VMARs may be operated on given `rights`.
    pub fn op_children_from_rights(rights: zx_rights_t) -> VmAddressRegionOpChildren {
        if (rights & ZX_RIGHT_OP_CHILDREN) == 0 {
            VmAddressRegionOpChildren::No
        } else {
            VmAddressRegionOpChildren::Yes
        }
    }
}
